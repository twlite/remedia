use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use remedia_core::{
    CancellationToken, InputSource, NullOutput, OpusEncoderOutput, PlaybackError, PlaybackState,
};
use remedia_discord::{ConnectionState, DiscordVoiceConnection, VoiceConnectionInfo};
use remedia_observability::ObservabilityHandle;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;
use tracing::instrument::WithSubscriber;

use crate::config::MediaAccessError;
use crate::protocol::{RpcError, RpcRequest};
use crate::state::{
    ClientSession, ConnectionContext, GuildSession, ServerInner, StateError, lock,
    playback_snapshot, playback_state_name,
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
        "node.info" => {
            decode_params::<EmptyParams>(request.params)?;
            Ok(json!({
                "nodeId": server.node_id,
                "version": env!("CARGO_PKG_VERSION"),
                "protocolVersion": crate::protocol::PROTOCOL_VERSION,
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
            let params: GuildParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
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
            let source = match params.source {
                PlaySource::File { path } => {
                    let authorized = server.config.authorize_file(&path).map_err(media_error)?;
                    InputSource::file(authorized)
                }
                PlaySource::Url { url } => {
                    let authorized = server
                        .config
                        .authorize_url(&url)
                        .await
                        .map_err(media_error)?;
                    InputSource::http_url(authorized)
                }
            };
            let generation = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
            let cancellation = CancellationToken::new();
            *lock(&guild.play_cancellation) = Some(cancellation.clone());
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

pub(crate) fn require_client(
    connection: &ConnectionContext,
) -> Result<Arc<ClientSession>, RpcError> {
    let session = connection.session().ok_or_else(|| {
        RpcError::new("unknown_session", "create or resume a client session first")
    })?;
    if session.principal != connection.principal || !session.attached_to(&connection.id) {
        return Err(RpcError::new(
            "unknown_session",
            "client session is not owned by this connection",
        ));
    }
    session.ensure_open().map_err(state_error)?;
    Ok(session)
}

pub(crate) fn decode_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|_| {
        RpcError::new(
            "invalid_request",
            "command params have invalid fields or types",
        )
    })
}

fn validate_snowflake(value: &str, field: &'static str) -> Result<u64, RpcError> {
    parse_snowflake(value)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            RpcError::new(
                "invalid_request",
                format!("{field} must be a positive decimal u64 string"),
            )
        })
}

fn parse_snowflake(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn valid_discord_voice_endpoint(endpoint: &str) -> bool {
    let endpoint = if endpoint.starts_with("wss://") || endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("wss://{endpoint}")
    };
    let Ok(endpoint) = url::Url::parse(&endpoint) else {
        return false;
    };
    let Some(host) = endpoint.host_str() else {
        return false;
    };
    endpoint.scheme() == "wss"
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.query().is_none()
        && endpoint.fragment().is_none()
        && (host.eq_ignore_ascii_case("discord.media")
            || host.to_ascii_lowercase().ends_with(".discord.media"))
}

fn ensure_param_guild(param_guild_id: &str, selected_guild_id: &str) -> Result<(), RpcError> {
    if param_guild_id == selected_guild_id {
        Ok(())
    } else {
        Err(RpcError::new(
            "invalid_request",
            "guildId changed while processing the request",
        ))
    }
}

fn state_error(error: StateError) -> RpcError {
    match error {
        StateError::UnknownSession => {
            RpcError::new("unknown_session", "session or player was not found")
        }
        StateError::InvalidState => {
            RpcError::new("invalid_state", "operation is invalid in the current state")
        }
        StateError::ResourceExhausted => RpcError::new(
            "resource_exhausted",
            "configured server capacity is exhausted",
        ),
        StateError::Cleanup => RpcError::new(
            "transport_failed",
            "session cleanup encountered a playback or transport failure",
        ),
    }
}

pub(crate) fn media_error(_error: MediaAccessError) -> RpcError {
    RpcError::new(
        "invalid_request",
        "media source is not allowed by server policy",
    )
}

fn spawn_observed<T>(
    future: impl Future<Output = T> + Send + 'static,
    observability: &ObservabilityHandle,
) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
{
    if let Some(dispatch) = observability.dispatch().cloned() {
        tokio::spawn(future.with_subscriber(dispatch))
    } else {
        tokio::spawn(future.with_subscriber(tracing::Dispatch::none()))
    }
}

fn playback_error(error: PlaybackError) -> RpcError {
    let code = playback_error_code(&error);
    let message = match code {
        "invalid_state" => "playback operation is invalid in the current state",
        "unsupported" => "playback operation is unsupported for this source",
        _ => "playback operation failed; inspect server diagnostics for details",
    };
    RpcError::new(code, message)
}

fn playback_error_code(error: &PlaybackError) -> &'static str {
    match error {
        PlaybackError::InvalidState { .. } => "invalid_state",
        PlaybackError::UnsupportedSeeking(_) | PlaybackError::SeekOutOfRange { .. } => {
            "unsupported"
        }
        PlaybackError::Cancelled => "invalid_state",
        _ => "playback_failed",
    }
}

pub(crate) fn stream_info_json(info: remedia_core::AudioStreamInfo) -> Value {
    json!({
        "streamIndex": info.stream_index,
        "codec": info.codec,
        "sampleRate": info.sample_rate,
        "channels": info.channels,
        "channelLayout": info.channel_layout,
        "durationMs": info.duration.map(|duration| duration.as_millis()),
        "bitRate": info.bit_rate,
        "metadata": info.metadata,
    })
}

/// Claim a monotonically increasing transport-operation generation from a client.
/// WebSocket handlers execute concurrently, so the server must not rely on task
/// scheduling to preserve request arrival order. Older clients omit the field;
/// those requests receive a server-generated generation for compatibility.
fn claim_voice_generation(generation: &AtomicU64, requested: Option<u64>) -> Result<u64, RpcError> {
    let Some(requested) = requested else {
        return Ok(generation.fetch_add(1, Ordering::AcqRel) + 1);
    };
    if requested == 0 {
        return Err(RpcError::new(
            "invalid_request",
            "transportGeneration must be a positive integer",
        ));
    }

    let mut current = generation.load(Ordering::Acquire);
    loop {
        if requested < current {
            return Err(RpcError::new(
                "invalid_state",
                "Discord voice operation was superseded by a newer transport generation",
            ));
        }
        if requested == current {
            return Ok(requested);
        }
        match generation.compare_exchange_weak(
            current,
            requested,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(requested),
            Err(actual) => current = actual,
        }
    }
}

fn playback_start_json(info: remedia_core::AudioStreamInfo, generation: u64) -> Value {
    let mut result = stream_info_json(info);
    result["generation"] = json!(generation);
    result
}

fn voice_state_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::NegotiatingDave => "negotiating_dave",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Disconnecting => "disconnecting",
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Failed => "failed",
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

mod player_control;
use player_control::*;

mod monitor;
use monitor::*;

mod statistics;
pub(crate) use statistics::node_stats;

mod params;
pub(crate) use params::*;

mod voice;
