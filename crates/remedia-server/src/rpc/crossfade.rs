//! RPC controls for independently staged playback.
use super::*;
use crate::state::StageScope;
use std::sync::atomic::AtomicU64;

static NEXT_STAGE_SCOPE: AtomicU64 = AtomicU64::new(1);
use remedia_core::{StageIdentity, StageSnapshot, StageState};

pub(super) async fn execute(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    let guild_id = request
        .params
        .get("guildId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("invalid_request", "guildId is required"))?
        .to_owned();
    validate_snowflake(&guild_id, "guildId")?;
    let client = require_client(&connection)?;
    let guild = server.get_guild(&client, &guild_id).map_err(state_error)?;
    guild.ensure_open().map_err(state_error)?;
    if !server.config.crossfade.enabled || guild.playback.crossfade_config().is_none() {
        return Err(RpcError::new(
            "feature_disabled",
            "crossfade is not enabled for this guild",
        ));
    }
    match request.command.as_str() {
        "player.prepare.ack" => {
            let params: PrepareAckParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            let generation = guild.source_generation.load(Ordering::Acquire);
            Ok(
                json!({"acknowledged": generation == params.expected_generation, "generation": generation}),
            )
        }
        "player.stage.state" => {
            decode_params::<GuildParams>(request.params)?;
            Ok(guild.playback.staged_source().map_or(Value::Null, |stage| {
                stage_json(stage, guild.source_generation.load(Ordering::Acquire))
            }))
        }
        "player.stage.clear" => {
            decode_params::<GuildParams>(request.params)?;
            guild.cancel_pending_stage();
            let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
            let playback = Arc::clone(&guild.playback);
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.clear_staged_source();
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "could not clear staged source"))?;
            Ok(json!({"cleared": true}))
        }
        "player.stage.next" => {
            let params: StageNextParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            if params.entry_id.trim().is_empty() || params.entry_id.len() > 256 {
                return Err(RpcError::new(
                    "invalid_request",
                    "entryId must be 1-256 characters",
                ));
            }
            // The staged timeline is addressed by generation. Reject an obviously outdated
            // request before running any plugin operation on its behalf.
            if params.expected_generation != guild.source_generation.load(Ordering::Acquire) {
                return Err(RpcError::new(
                    "stale_generation",
                    "playback generation changed before staging",
                ));
            }

            // Own a cancellation scope for this preparation. A newer revision, a clear, a
            // stop, a replacement play, a guild close or shutdown cancels it; so does a
            // generation change observed after the source resolves.
            let scope_id = NEXT_STAGE_SCOPE.fetch_add(1, Ordering::Relaxed);
            let scope = StageScope {
                id: scope_id,
                token: tokio_util::sync::CancellationToken::new(),
                expected_generation: params.expected_generation,
                entry_id: params.entry_id.clone(),
                queue_revision: params.queue_revision,
                source_revision: params.source_revision,
            };
            {
                let mut pending = lock(&guild.stage_scope);

                if let Some(existing) = pending.as_ref() {
                    if existing.expected_generation == scope.expected_generation
                        && (scope.queue_revision < existing.queue_revision
                            || (scope.queue_revision == existing.queue_revision
                                && scope.entry_id == existing.entry_id
                                && scope.source_revision < existing.source_revision))
                    {
                        return Err(RpcError::new(
                            "stale_generation",
                            "a newer staged source is already being prepared",
                        ));
                    }

                    existing.token.cancel();
                }

                *pending = Some(scope.clone());
            }
            let token = scope.token.clone();
            // Dropping this guard (success, error, request timeout or socket loss) cancels the
            // preparation and forgets its scope, so no operation outlives its request.
            let _scope_guard = StageScopeGuard {
                guild: Arc::clone(&guild),
                id: scope_id,
                token: token.clone(),
            };
            let resolved = match resolve_source(&server, &connection, params.source, &token).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    return Err(error);
                }
            };
            let ResolvedInput { source, guard } = resolved;
            let mut media_guard = guard;
            let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
            let current = guild.source_generation.load(Ordering::Acquire);

            guild.ensure_open().map_err(state_error)?;
            // A completion which outlived its scope or timeline must never be staged.
            if token.is_cancelled() || params.expected_generation != current {
                return Err(RpcError::new(
                    "stale_generation",
                    "playback generation changed before staging",
                ));
            }
            let generation = current;
            let expires_at_ms = if media_guard.is_some() {
                None
            } else {
                params.expires_at_ms
            };
            let playback = Arc::clone(&guild.playback);
            let identity = StageIdentity {
                expected_generation: playback.source_generation(),
                entry_id: params.entry_id,
                queue_revision: params.queue_revision,
                source_revision: params.source_revision,
            };
            let result = tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.stage_next_with_expiration(source, identity, expires_at_ms)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "staging worker failed"))?;
            // The staged decoder owns a successfully started transfer; otherwise cancel it.
            if let Some(guard) = media_guard.take()
                && result.is_ok()
            {
                guard.disarm();
            }
            Ok(stage_json(result.map_err(playback_error)?, generation))
        }
        _ => Err(RpcError::new(
            "invalid_request",
            "unknown staging operation",
        )),
    }
}

/// Cancels a staged-source preparation and forgets its scope when the request ends.
struct StageScopeGuard {
    guild: Arc<GuildSession>,
    id: u64,
    token: tokio_util::sync::CancellationToken,
}

impl Drop for StageScopeGuard {
    fn drop(&mut self) {
        self.token.cancel();

        let mut pending = lock(&self.guild.stage_scope);

        if pending.as_ref().is_some_and(|scope| scope.id == self.id) {
            pending.take();
        }
    }
}

fn stage_json(stage: StageSnapshot, generation: u64) -> Value {
    json!({
        "entryId": stage.identity.entry_id,
        "expectedGeneration": generation,
        "queueRevision": stage.identity.queue_revision,
        "sourceRevision": stage.identity.source_revision,
        "state": match stage.state {
            StageState::Preparing => "preparing",
            StageState::Ready => "ready",
            StageState::Failed => "failed",
        },
        "bufferedMs": stage.buffered_samples as u64 / 48,
        "source": stage.info.map(stream_info_json),
        "error": stage.error,
    })
}
