//! RPC controls for independently staged playback.
use super::*;
use remedia_core::{CrossfadeConfig, StageIdentity, StageSnapshot, StageState};

pub(super) async fn execute(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    let guild_id = request.params.get("guildId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("invalid_request", "guildId is required"))?
        .to_owned();
    validate_snowflake(&guild_id, "guildId")?;
    let client = require_client(&connection)?;
    let guild = server.get_guild(&client, &guild_id).map_err(state_error)?;
    guild.ensure_open().map_err(state_error)?;
    if !server.config.crossfade.enabled || guild.playback.crossfade_config().is_none() {
        return Err(RpcError::new("feature_disabled", "crossfade is not enabled for this guild"));
    }
    match request.command.as_str() {
        "player.prepare.ack" => {
            let params: PrepareAckParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            let generation = guild.source_generation.load(Ordering::Acquire);
            Ok(json!({"acknowledged": generation == params.expected_generation, "generation": generation}))
        }
        "player.stage.state" => {
            decode_params::<GuildParams>(request.params)?;
            Ok(guild.playback.staged_source().map_or(Value::Null, |stage| stage_json(stage, guild.source_generation.load(Ordering::Acquire))))
        }
        "player.stage.clear" => {
            decode_params::<GuildParams>(request.params)?;
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
                return Err(RpcError::new("invalid_request", "entryId must be 1-256 characters"));
            }
            let source = match params.source {
                PlaySource::File { path } => {
                    let file = server.config.authorize_file(&path).map_err(media_error)?;
                    InputSource::file(file)
                }
                PlaySource::Url { url } => {
                    let url = server.config.authorize_url(&url).await.map_err(media_error)?;
                    InputSource::http_url(url)
                }
            };
            let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            let generation = guild.source_generation.load(Ordering::Acquire);
            if params.expected_generation != generation {
                return Err(RpcError::new("stale_generation", "playback generation changed before staging"));
            }
            let playback = Arc::clone(&guild.playback);
            let identity = StageIdentity {
                expected_generation: playback.source_generation(),
                entry_id: params.entry_id,
                queue_revision: params.queue_revision,
                source_revision: params.source_revision,
            };
            let result = tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.stage_next(source, identity)
            }).await.map_err(|_| RpcError::new("playback_failed", "staging worker failed"))?
              .map_err(playback_error)?;
            Ok(stage_json(result, generation))
        }
        _ => Err(RpcError::new("invalid_request", "unknown staging operation")),
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
