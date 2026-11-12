use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use remedia_core::{
    CancellationToken, NullOutput, OpusEncoderOutput, PlaybackError, PlaybackState,
};
use remedia_discord::{ConnectionState, DiscordVoiceConnection, VoiceConnectionInfo};
use remedia_observability::ObservabilityHandle;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;
use tracing::instrument::WithSubscriber;

use crate::config::{CrossfadeCurve, MediaAccessError, engine_curve};
use crate::plugin_host::{MediaGuard, OperationRef, PluginError, PluginErrorKind};
use crate::protocol::{RpcError, RpcRequest};
use crate::state::{
    ClientSession, ConnectionContext, GuildSession, PlayCancellation, ServerInner, StateError,
    lock, playback_snapshot, playback_state_name,
};

pub(crate) async fn execute(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    match request.command.as_str() {
        "media.probe" | "media.probe.cancel" => {
            crate::probe::execute(server, connection, request).await
        }
        "plugin.list" | "plugin.execute" | "plugin.cancel" => {
            plugin::execute(server, connection, request).await
        }
        "node.info" => {
            decode_params::<EmptyParams>(request.params)?;
            Ok(json!({
                "nodeId": server.node_id,
                "version": env!("REMEDIA_BUILD_VERSION"),
                "commit": env!("REMEDIA_BUILD_COMMIT"),
                "protocolVersion": crate::protocol::PROTOCOL_VERSION,
                "capabilities": {
                    "pluginOperations": server.plugin_hosts.capability(),
                    "crossfade": {
                        "configured": server.config.crossfade.enabled,
                        "available": server.config.crossfade.enabled,
                        "durationMs": server.config.crossfade.duration_ms,
                        "prepareBeforeMs": server.config.crossfade.prepare_before_ms,
                        "curve": server.config.crossfade.curve.as_str(),
                        "curves": [
                            CrossfadeCurve::EqualPower.as_str(),
                            CrossfadeCurve::Adaptive.as_str(),
                        ],
                    }
                }
            }))
        }
        "node.protocol_version" => {
            decode_params::<EmptyParams>(request.params)?;
            Ok(json!({ "version": crate::protocol::PROTOCOL_VERSION }))
        }
        "node.health" => {
            decode_params::<EmptyParams>(request.params)?;
            Ok(json!({ "status": server.health() }))
        }
        "node.stats" => {
            decode_params::<EmptyParams>(request.params)?;
            Ok(node_stats(&server))
        }
        "client.create" => {
            decode_params::<EmptyParams>(request.params)?;
            let _gate = connection.session_control.lock().await;
            if connection.session().is_some() {
                return Err(RpcError::new(
                    "invalid_state",
                    "this WebSocket already controls a client session",
                ));
            }
            let (session, credential) = server
                .create_client(
                    &connection.principal,
                    connection.bot_user_id,
                    &connection.id,
                    server.config.limits.event_buffer,
                )
                .map_err(state_error)?;
            connection.set_session(Some(Arc::clone(&session)));
            Ok(json!({
                "sessionId": session.id,
                "resumeCredential": credential,
                "botUserId": session.bot_user_id.to_string(),
            }))
        }
        "client.list" => {
            decode_params::<EmptyParams>(request.params)?;
            Ok(json!({
                "sessions": server.list_clients(&connection.principal, connection.bot_user_id)
            }))
        }
        "client.resume" => {
            let params: ResumeParams = decode_params(request.params)?;
            let _gate = connection.session_control.lock().await;
            if connection.session().is_some() {
                return Err(RpcError::new(
                    "invalid_state",
                    "this WebSocket already controls a client session",
                ));
            }
            let (session, credential) = server
                .resume_client(
                    &params.session_id,
                    &params.resume_credential,
                    &connection.principal,
                    connection.bot_user_id,
                    &connection.id,
                )
                .map_err(state_error)?;
            connection.set_session(Some(Arc::clone(&session)));
            Ok(json!({
                "sessionId": session.id,
                "resumeCredential": credential,
                "snapshot": session.snapshot(),
            }))
        }
        "client.close" => {
            decode_params::<EmptyParams>(request.params)?;
            let _gate = connection.session_control.lock().await;
            let session = require_client(&connection)?;
            let id = session.id.clone();
            connection.set_session(None);
            // Cleanup outlives an RPC timeout or socket drop once it has started.
            let cleanup_server = Arc::clone(&server);
            let cleanup_id = id.clone();
            let cleanup = spawn_observed(
                async move { cleanup_server.close_client(&cleanup_id).await },
                &server.observability,
            );
            let cleanup_result = cleanup
                .await
                .map_err(|_| RpcError::new("transport_failed", "session cleanup task failed"))?;
            cleanup_result.map_err(state_error)?;
            Ok(json!({ "closed": true, "sessionId": id }))
        }
        "guild.create" => {
            let params: GuildCreateParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            if let Some(crossfade) = &params.crossfade {
                if crossfade.enabled {
                    if !server.config.crossfade.enabled {
                        return Err(RpcError::new(
                            "feature_disabled",
                            "crossfade is disabled by the server configuration",
                        ));
                    }
                    let curve = match crossfade.curve.as_deref() {
                        None => server.config.crossfade.curve,
                        Some(name) => CrossfadeCurve::from_name(name).ok_or_else(|| {
                            RpcError::new(
                                "invalid_request",
                                "unsupported crossfade curve; use equalPower or adaptive",
                            )
                        })?,
                    };
                    let config = remedia_core::CrossfadeConfig::new(
                        crossfade
                            .duration_ms
                            .unwrap_or(server.config.crossfade.duration_ms),
                        crossfade
                            .prepare_before_ms
                            .unwrap_or(server.config.crossfade.prepare_before_ms),
                    )
                    .ok_or_else(|| {
                        RpcError::new(
                            "invalid_request",
                            "invalid crossfade duration or preparation lead",
                        )
                    })?
                    .with_curve(engine_curve(curve));
                    // The session is constructed below; install this configuration
                    // before playback is permitted to start.
                    let session = require_client(&connection)?;
                    let guild = server
                        .create_guild(&session, params.guild_id)
                        .map_err(state_error)?;
                    guild
                        .playback
                        .configure_crossfade(Some(config))
                        .map_err(playback_error)?;
                    let snapshot = guild.snapshot();
                    session.emit("guild.created", snapshot.clone());
                    return Ok(snapshot);
                }
                if crossfade.duration_ms.is_some()
                    || crossfade.prepare_before_ms.is_some()
                    || crossfade.curve.is_some()
                {
                    return Err(RpcError::new(
                        "invalid_request",
                        "crossfade options require enabled: true",
                    ));
                }
            }
            let session = require_client(&connection)?;
            let guild = server
                .create_guild(&session, params.guild_id)
                .map_err(state_error)?;
            let snapshot = guild.snapshot();
            session.emit("guild.created", snapshot.clone());
            Ok(snapshot)
        }
        "guild.list" => {
            decode_params::<EmptyParams>(request.params)?;
            let session = require_client(&connection)?;
            session.ensure_open().map_err(state_error)?;
            Ok(json!({ "guilds": session.guild_snapshots() }))
        }
        "guild.get" => {
            let params: GuildParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            Ok(guild.snapshot())
        }
        "guild.destroy" => {
            let params: GuildParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let cleanup_server = Arc::clone(&server);
            let cleanup_session = Arc::clone(&session);
            let cleanup_guild_id = params.guild_id.clone();
            spawn_observed(
                async move {
                    cleanup_server
                        .destroy_guild(&cleanup_session, &cleanup_guild_id)
                        .await
                },
                &server.observability,
            )
            .await
            .map_err(|_| RpcError::new("transport_failed", "guild cleanup task failed"))?
            .map_err(state_error)?;
            session.emit("guild.destroyed", json!({ "guildId": params.guild_id }));
            Ok(json!({ "destroyed": true }))
        }
        "voice.connect" | "voice.state" | "voice.suspend" | "voice.disconnect" | "voice.fail" => {
            voice::execute(server, connection, request).await
        }
        "player.play" => {
            let params: PlayParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            if matches!(
                guild.playback.state(),
                PlaybackState::Playing
                    | PlaybackState::Paused
                    | PlaybackState::Starting
                    | PlaybackState::Pausing
                    | PlaybackState::Resuming
            ) {
                return Err(RpcError::new(
                    "invalid_state",
                    "stop the active source before starting another",
                ));
            }
            // A staged preparation belongs to the timeline being replaced.
            guild.cancel_pending_stage();

            // Install the cancellation scope before resolving the source so a concurrent
            // stop or skip can interrupt a pending plugin operation, not just the decoder.
            let play_cancellation = PlayCancellation::new();
            *lock(&guild.play_cancellation) = Some(play_cancellation.clone());
            // If this request is dropped (timeout or socket loss) while the source resolves,
            // cancel the pending operation instead of leaving it running.
            let mut resolving = CancelOnDrop(Some(play_cancellation.clone()));
            let resolved = match resolve_source(
                &server,
                &connection,
                params.source,
                &play_cancellation.scope,
            )
            .await
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    resolving.0 = None;
                    lock(&guild.play_cancellation).take();
                    return Err(error);
                }
            };
            resolving.0 = None;

            // Refuse a result which completed after the request was cancelled or the guild closed.
            if play_cancellation.scope.is_cancelled() || guild.closed.load(Ordering::Acquire) {
                lock(&guild.play_cancellation).take();
                return Err(RpcError::new(
                    "cancelled",
                    "playback was cancelled before the source was ready",
                ));
            }
            let ResolvedInput { source, guard } = resolved;
            let mut media_guard = guard;
            let generation = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
            let cancellation = play_cancellation.core.clone();
            let mut cancel_guard = PlayCancelOnDrop {
                guild: Arc::downgrade(&guild),
                generation,
                playback: Arc::clone(&guild.playback),
                cancellation: cancellation.clone(),
                completed: false,
            };
            let playback = Arc::clone(&guild.playback);
            let output = if let Some(voice) = lock(&guild.voice).clone() {
                if !matches!(
                    voice.state(),
                    ConnectionState::Connected | ConnectionState::Reconnecting
                ) {
                    lock(&guild.play_cancellation).take();
                    drop(media_guard.take());
                    return Err(RpcError::new(
                        "transport_failed",
                        "Discord voice transport is not connected",
                    ));
                }
                OpusEncoderOutput::new_with_config_and_observability(
                    guild.voice_sender_router.sink(),
                    server.config.opus,
                    server.observability.clone(),
                )
                .map_err(|_| {
                    RpcError::new("playback_failed", "could not initialize the Opus output")
                })?
            } else {
                // A player without Discord consumes decoded frames into a null output. This is
                // useful for server-side validation and tests; it does not pretend to be audible.
                let playback = Arc::clone(&guild.playback);
                let cancellation_for_start = cancellation.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _operation = operation;
                    playback.play_with_cancellation(
                        source,
                        NullOutput::new(),
                        cancellation_for_start,
                    )
                })
                .await
                .map_err(|_| RpcError::new("playback_failed", "playback worker failed to start"))?;
                cancel_guard.completed = true;
                lock(&guild.play_cancellation).take();
                settle_media_guard(&mut media_guard, result.is_ok());
                let info = result.map_err(|error| {
                    let _ = guild.claim_terminal_event(generation);
                    session.emit(
                        "player.failed",
                        json!({"guildId": guild.guild_id, "generation": generation, "code": playback_error_code(&error)}),
                    );
                    RpcError::new("playback_failed", "media playback could not be started")
                })?;
                let core_generation = guild.playback.completion_generation();
                session.emit(
                    "player.started",
                    json!({"guildId": guild.guild_id, "generation": generation, "source": stream_info_json(info.clone())}),
                );
                spawn_playback_monitor(
                    Arc::clone(&guild),
                    Arc::downgrade(&session),
                    generation,
                    core_generation,
                );
                return Ok(playback_start_json(info, generation));
            };
            let cancellation_for_start = cancellation.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.play_with_cancellation(source, output, cancellation_for_start)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed to start"))?;
            cancel_guard.completed = true;
            settle_media_guard(&mut media_guard, result.is_ok());
            let info = match result {
                Ok(info) => info,
                Err(error) => {
                    lock(&guild.play_cancellation).take();
                    let _ = guild.claim_terminal_event(generation);
                    session.emit("player.failed", json!({"guildId": guild.guild_id, "generation": generation, "code": playback_error_code(&error)}));
                    return Err(RpcError::new(
                        "playback_failed",
                        "media playback could not be started",
                    ));
                }
            };
            let core_generation = guild.playback.completion_generation();
            session.emit(
                "player.started",
                json!({"guildId": guild.guild_id, "generation": generation, "source": stream_info_json(info.clone())}),
            );
            spawn_playback_monitor(
                Arc::clone(&guild),
                Arc::downgrade(&session),
                generation,
                core_generation,
            );
            Ok(playback_start_json(info, generation))
        }
        "player.stage.next" | "player.stage.clear" | "player.stage.state"
        | "player.prepare.ack" => crossfade::execute(server, connection, request).await,
        "player.pause"
        | "player.resume"
        | "player.stop"
        | "player.seek"
        | "player.filter.set"
        | "player.filter.clear"
        | "player.volume.set" => execute_player_control(server, connection, request).await,
        "player.state" => {
            let params: GuildParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            Ok(playback_snapshot(
                &guild.playback,
                guild.source_generation.load(Ordering::Acquire),
            ))
        }
        _ => Err(RpcError::new("invalid_request", "unknown command")),
    }
}

/// Release a plugin-host media transfer if playback did not start; otherwise the
/// decoder owns it and closes it when playback ends.
fn settle_media_guard(guard: &mut Option<MediaGuard>, started: bool) {
    if let Some(guard) = guard.take()
        && started
    {
        guard.disarm();
    }
}

/// Cancels a playback scope when dropped unless it was cleared first.
struct CancelOnDrop(Option<PlayCancellation>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = self.0.take() {
            cancellation.cancel();
        }
    }
}

struct PlayCancelOnDrop {
    guild: std::sync::Weak<GuildSession>,
    generation: u64,
    playback: Arc<remedia_core::PlaybackSession>,
    cancellation: CancellationToken,
    completed: bool,
}

impl Drop for PlayCancelOnDrop {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.cancellation.cancel();
        let Some(guild) = self.guild.upgrade() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            let generation = self.generation;
            let playback = Arc::clone(&self.playback);
            tokio::spawn(async move {
                let _operation = Arc::clone(&guild.playback_operations).lock_owned().await;
                if guild.source_generation.load(Ordering::Acquire) == generation {
                    let _ = tokio::task::spawn_blocking(move || playback.stop()).await;
                    lock(&guild.play_cancellation).take();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;

mod crossfade;
mod player_control;
use player_control::*;

mod monitor;
use monitor::*;

mod statistics;
pub(crate) use statistics::node_stats;

mod params;
pub(crate) use params::*;

mod plugin;

mod voice;

mod helpers;
use helpers::*;
pub(crate) use helpers::{
    ResolvedInput, decode_params, media_error, require_client, resolve_source, stream_info_json,
};
