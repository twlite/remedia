use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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

use crate::config::{AuthUser, MediaAccessError};
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
            let user = AuthUser {
                username: connection.principal.clone(),
                password: String::new(),
                discord_bot_user_id: connection.bot_user_id,
            };
            let (session, credential) = server
                .create_client(&user, &connection.id, server.config.limits.event_buffer)
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
            Ok(json!({ "sessions": server.list_clients(&connection.principal) }))
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
        "voice.connect" => {
            let params: VoiceConnectParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            validate_snowflake(&params.channel_id, "channelId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let _voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            if lock(&guild.voice).as_ref().is_some_and(|voice| {
                !matches!(
                    voice.state(),
                    ConnectionState::Disconnected | ConnectionState::Failed
                )
            }) {
                return Err(RpcError::new(
                    "invalid_state",
                    "a voice transport is already associated with this guild session",
                ));
            }
            if params.session_id.is_empty()
                || params.session_id.len() > 1024
                || params.token.is_empty()
                || params.token.len() > 4096
                || params.endpoint.is_empty()
                || params.endpoint.len() > 2048
                || !valid_discord_voice_endpoint(&params.endpoint)
            {
                return Err(RpcError::new(
                    "invalid_request",
                    "voice session credentials or Discord endpoint are malformed",
                ));
            }
            let connection_info = VoiceConnectionInfo {
                guild_id: parse_snowflake(&params.guild_id).expect("validated snowflake"),
                channel_id: parse_snowflake(&params.channel_id).expect("validated snowflake"),
                user_id: guild.bot_user_id,
                session_id: params.session_id,
                token: params.token,
                endpoint: params.endpoint,
            };
            session.emit(
                "voice.state_changed",
                json!({"guildId": guild.guild_id, "state": "connecting"}),
            );
            let voice = match DiscordVoiceConnection::connect_with_observability(
                connection_info,
                server.observability.clone(),
            )
            .await
            {
                Ok(voice) => voice,
                Err(_) => {
                    session.emit(
                        "voice.failed",
                        json!({"guildId": guild.guild_id, "code": "transport_failed"}),
                    );
                    return Err(RpcError::new(
                        "transport_failed",
                        "Discord voice connection failed",
                    ));
                }
            };
            if guild.closed.load(Ordering::Acquire) || session.closed.load(Ordering::Acquire) {
                let _ = voice.disconnect().await;
                return Err(RpcError::new(
                    "unknown_session",
                    "guild session was closed during voice connection",
                ));
            }
            let initial_state = voice_state_name(voice.state());
            *lock(&guild.voice) = Some(voice);
            session.emit(
                "voice.state_changed",
                json!({"guildId": guild.guild_id, "state": initial_state}),
            );
            spawn_voice_monitor(Arc::clone(&guild), Arc::downgrade(&session));
            Ok(json!({ "guildId": guild.guild_id, "state": initial_state }))
        }
        "voice.state" => {
            let params: GuildParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let voice = lock(&guild.voice);
            Ok(json!({
                "state": voice.as_ref().map(|voice| voice_state_name(voice.state())).unwrap_or("disconnected"),
                "failed": voice.as_ref().is_some_and(|voice| voice.failure().is_some()),
            }))
        }
        "voice.disconnect" => {
            let params: GuildParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            stop_playback(&guild).await?;
            let voice = { lock(&guild.voice).take() };
            if let Some(voice) = voice {
                let disconnect = tokio::spawn(async move {
                    let _voice_operation = voice_operation;
                    voice.disconnect().await
                });
                disconnect
                    .await
                    .map_err(|_| RpcError::new("transport_failed", "voice cleanup task failed"))?
                    .map_err(|_| {
                        session.emit(
                            "voice.failed",
                            json!({"guildId": guild.guild_id, "code": "transport_failed"}),
                        );
                        RpcError::new("transport_failed", "Discord voice disconnection failed")
                    })?;
            }
            session.emit(
                "voice.state_changed",
                json!({"guildId": guild.guild_id, "state": "disconnected"}),
            );
            Ok(json!({ "guildId": guild.guild_id, "state": "disconnected" }))
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
                let sender = voice.opus_sender();
                OpusEncoderOutput::new_with_observability(sender, server.observability.clone())
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
                    session.emit(
                        "player.failed",
                        json!({"guildId": guild.guild_id, "generation": generation, "code": playback_error_code(&error)}),
                    );
                    RpcError::new("playback_failed", "media playback could not be started")
                })?;
                spawn_playback_monitor(Arc::clone(&guild), Arc::downgrade(&session), generation);
                session.emit(
                    "player.started",
                    json!({"guildId": guild.guild_id, "generation": generation, "source": stream_info_json(info.clone())}),
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
                    session.emit("player.failed", json!({"guildId": guild.guild_id, "generation": generation, "code": playback_error_code(&error)}));
                    return Err(RpcError::new(
                        "playback_failed",
                        "media playback could not be started",
                    ));
                }
            };
            spawn_playback_monitor(Arc::clone(&guild), Arc::downgrade(&session), generation);
            session.emit(
                "player.started",
                json!({"guildId": guild.guild_id, "generation": generation, "source": stream_info_json(info.clone())}),
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

async fn execute_player_control(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    let guild_id = request
        .params
        .get("guildId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new("invalid_request", "guildId must be a decimal string"))?
        .to_owned();
    validate_snowflake(&guild_id, "guildId")?;
    let session = require_client(&connection)?;
    let guild = server.get_guild(&session, &guild_id).map_err(state_error)?;

    if request.command == "player.stop"
        && let Some(cancellation) = lock(&guild.play_cancellation).as_ref()
    {
        cancellation.cancel();
    }
    let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
    guild.ensure_open().map_err(state_error)?;
    match request.command.as_str() {
        "player.pause" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.pause()
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            session.emit("player.paused", json!({"guildId": guild.guild_id}));
            Ok(json!({"state": playback_state_name(guild.playback.state())}))
        }
        "player.resume" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.resume()
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            session.emit("player.resumed", json!({"guildId": guild.guild_id}));
            Ok(json!({"state": playback_state_name(guild.playback.state())}))
        }
        "player.stop" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.stop()
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            lock(&guild.play_cancellation).take();
            session.emit(
                "player.stopped",
                json!({
                    "guildId": guild.guild_id,
                    "generation": guild.source_generation.load(Ordering::Acquire),
                }),
            );
            Ok(json!({"state": playback_state_name(guild.playback.state())}))
        }
        "player.seek" => {
            let params: SeekParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            let playback = Arc::clone(&guild.playback);
            let position = Duration::from_millis(params.position_ms);
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.seek(position)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            session.emit(
                "player.seeked",
                json!({"guildId": guild.guild_id, "positionMs": params.position_ms}),
            );
            Ok(playback_snapshot(
                &guild.playback,
                guild.source_generation.load(Ordering::Acquire),
            )["position"]
                .clone())
        }
        "player.filter.set" => {
            let params: FilterSetParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            let playback = Arc::clone(&guild.playback);
            let graph = params.graph;
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.set_filter_graph(Some(&graph))
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "filtergraph worker failed"))?
            .map_err(playback_error)?;
            session.emit(
                "player.filter_changed",
                json!({"guildId": guild.guild_id, "configured": true}),
            );
            Ok(json!({"filterGraph": guild.playback.filter_graph()}))
        }
        "player.filter.clear" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.set_filter_graph(None)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "filtergraph worker failed"))?
            .map_err(playback_error)?;
            session.emit(
                "player.filter_changed",
                json!({"guildId": guild.guild_id, "configured": false}),
            );
            Ok(json!({"filterGraph": null}))
        }
        "player.volume.set" => {
            let params: VolumeParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            if !params.gain.is_finite() || !(0.0..=2.0).contains(&params.gain) {
                return Err(RpcError::new(
                    "invalid_request",
                    "gain must be a finite number from 0.0 through 2.0",
                ));
            }
            let playback = Arc::clone(&guild.playback);
            let gain = params.gain as f32;
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.set_volume(gain)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "volume worker failed"))?
            .map_err(playback_error)?;
            session.emit(
                "player.volume_changed",
                json!({"guildId": guild.guild_id, "gain": params.gain}),
            );
            Ok(json!({"volume": guild.playback.volume()}))
        }
        _ => Err(RpcError::new("invalid_request", "unknown player command")),
    }
}

async fn stop_playback(guild: &Arc<GuildSession>) -> Result<(), RpcError> {
    if let Some(cancellation) = lock(&guild.play_cancellation).as_ref() {
        cancellation.cancel();
    }
    let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
    let playback = Arc::clone(&guild.playback);
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        playback.stop()
    })
    .await
    .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
    .map_err(playback_error)?;
    lock(&guild.play_cancellation).take();
    Ok(())
}

fn require_client(connection: &ConnectionContext) -> Result<Arc<ClientSession>, RpcError> {
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

fn decode_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
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

fn media_error(_error: MediaAccessError) -> RpcError {
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

fn stream_info_json(info: remedia_core::AudioStreamInfo) -> Value {
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

fn spawn_playback_monitor(
    guild: Arc<GuildSession>,
    session: std::sync::Weak<ClientSession>,
    generation: u64,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if guild.closed.load(Ordering::Acquire) {
                return;
            }
            match guild.playback.state() {
                PlaybackState::Completed => {
                    if let Some(session) = session.upgrade()
                        && guild.source_generation.load(Ordering::Acquire) == generation
                    {
                        session.emit(
                            "player.completed",
                            json!({"guildId": guild.guild_id, "generation": generation}),
                        );
                    }
                    lock(&guild.play_cancellation).take();
                    return;
                }
                PlaybackState::Failed => {
                    if let Some(session) = session.upgrade()
                        && guild.source_generation.load(Ordering::Acquire) == generation
                    {
                        session.emit("player.failed", json!({"guildId": guild.guild_id, "generation": generation, "code": "playback_failed"}));
                    }
                    lock(&guild.play_cancellation).take();
                    return;
                }
                PlaybackState::Stopped => {
                    lock(&guild.play_cancellation).take();
                    return;
                }
                _ => {}
            }
        }
    });
}

fn spawn_voice_monitor(guild: Arc<GuildSession>, session: std::sync::Weak<ClientSession>) {
    tokio::spawn(async move {
        let mut previous = lock(&guild.voice)
            .as_ref()
            .map(DiscordVoiceConnection::state);
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if guild.closed.load(Ordering::Acquire) {
                return;
            }
            let state = lock(&guild.voice)
                .as_ref()
                .map(DiscordVoiceConnection::state);
            if state != previous {
                if let (Some(session), Some(state)) = (session.upgrade(), state) {
                    session.emit(
                        "voice.state_changed",
                        json!({"guildId": guild.guild_id, "state": voice_state_name(state)}),
                    );
                    if state == ConnectionState::Failed {
                        session.emit(
                            "voice.failed",
                            json!({"guildId": guild.guild_id, "code": "transport_failed"}),
                        );
                    }
                }
                previous = state;
            }
            if matches!(
                state,
                None | Some(ConnectionState::Disconnected | ConnectionState::Failed)
            ) {
                return;
            }
        }
    });
}

fn node_stats(server: &ServerInner) -> Value {
    let (processing_failed, transport_failed) = {
        let sessions = lock(&server.sessions);
        sessions
            .values()
            .flat_map(|session| lock(&session.guilds).values().cloned().collect::<Vec<_>>())
            .fold(
                (false, false),
                |(processing_failed, transport_failed), guild| {
                    let playback_failed = guild.playback.state() == PlaybackState::Failed;
                    let voice_failed = lock(&guild.voice)
                        .as_ref()
                        .is_some_and(|voice| voice.state() == ConnectionState::Failed);
                    (
                        processing_failed || playback_failed,
                        transport_failed || voice_failed,
                    )
                },
            )
    };
    json!({
        "nodeId": server.node_id,
        "version": env!("CARGO_PKG_VERSION"),
        "protocolVersion": crate::protocol::PROTOCOL_VERSION,
        "uptimeMs": server.started_at.elapsed().as_millis(),
        "connections": {"active": server.active_connections.load(Ordering::Acquire), "maximum": server.config.limits.max_connections},
        "players": {"active": server.active_player_count(), "maximum": server.config.limits.max_total_players},
        "cpu": {"availableParallelism": std::thread::available_parallelism().ok().map(usize::from), "loadAverage1m": load_average_1m()},
        "memory": {"residentBytes": resident_memory_bytes()},
        "transportHealth": if transport_failed { "degraded" } else { "healthy" },
        "processingHealth": if processing_failed { "degraded" } else { "healthy" },
    })
}

fn load_average_1m() -> Option<f64> {
    let contents = std::fs::read_to_string("/proc/loadavg").ok()?;
    contents.split_whitespace().next()?.parse().ok()
}

fn resident_memory_bytes() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = contents.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ResumeParams {
    session_id: String,
    resume_credential: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GuildParams {
    guild_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct VoiceConnectParams {
    guild_id: String,
    channel_id: String,
    session_id: String,
    token: String,
    endpoint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PlayParams {
    guild_id: String,
    source: PlaySource,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum PlaySource {
    File { path: String },
    Url { url: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SeekParams {
    guild_id: String,
    position_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FilterSetParams {
    guild_id: String,
    graph: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct VolumeParams {
    guild_id: String,
    gain: f64,
}

#[cfg(test)]
mod tests {
    use super::{
        load_average_1m, parse_snowflake, resident_memory_bytes, valid_discord_voice_endpoint,
    };

    #[test]
    fn snowflakes_are_decimal_and_remain_u64() {
        assert_eq!(parse_snowflake("18446744073709551614"), Some(u64::MAX - 1));
        assert_eq!(parse_snowflake("1.2"), None);
        assert_eq!(parse_snowflake("18446744073709551616"), None);
    }

    #[test]
    fn host_stats_are_optional_and_well_formed_when_available() {
        assert!(resident_memory_bytes().is_none_or(|memory| memory > 0));
        assert!(load_average_1m().is_none_or(|load| load.is_finite() && load >= 0.0));
    }

    #[test]
    fn voice_endpoints_are_secure_and_scoped_to_discord_media() {
        assert!(valid_discord_voice_endpoint("us-east123.discord.media"));
        assert!(valid_discord_voice_endpoint(
            "wss://voice.discord.media:2048"
        ));
        assert!(!valid_discord_voice_endpoint("ws://voice.discord.media"));
        assert!(!valid_discord_voice_endpoint("wss://127.0.0.1:8080"));
        assert!(!valid_discord_voice_endpoint(
            "wss://discord.media.attacker.test"
        ));
        assert!(!valid_discord_voice_endpoint(
            "wss://user@voice.discord.media"
        ));
    }
}
