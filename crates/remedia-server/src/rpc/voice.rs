use super::*;
pub(super) async fn execute(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    match request.command.as_str() {
        "voice.connect" => {
            let params: VoiceConnectParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            validate_snowflake(&params.channel_id, "channelId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let request_generation = claim_voice_generation(
                &guild.voice_request_generation,
                params.transport_generation,
            )?;
            let _voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            if guild.voice_request_generation.load(Ordering::Acquire) != request_generation {
                return Err(RpcError::new(
                    "invalid_state",
                    "Discord voice connection attempt was superseded",
                ));
            }
            if guild.voice_terminal_generation.load(Ordering::Acquire) >= request_generation {
                return Err(RpcError::new(
                    "invalid_state",
                    "Discord voice connection attempt was cancelled",
                ));
            }
            {
                let voice = lock(&guild.voice);
                if guild.voice_connected_generation.load(Ordering::Acquire) == request_generation
                    && let Some(voice) = voice.as_ref()
                {
                    let state = voice_state_name(voice.state());
                    if !matches!(
                        voice.state(),
                        ConnectionState::Disconnected | ConnectionState::Failed
                    ) {
                        return Ok(
                            json!({ "guildId": guild.guild_id, "state": state, "alreadyConnected": true }),
                        );
                    }
                }
            }
            let voice_generation = guild.voice_generation.fetch_add(1, Ordering::AcqRel) + 1;
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
            let previous_voice = { lock(&guild.voice).take() };
            let previous_voice_state = previous_voice
                .as_ref()
                .map(|voice| voice_state_name(voice.state()))
                .unwrap_or("disconnected");
            let recovery =
                previous_voice.is_some() || guild.voice_reconnecting.swap(true, Ordering::AcqRel);
            guild.voice_reconnecting.store(recovery, Ordering::Release);
            guild.voice_sender_router.detach();
            tracing::info!(
                event = "discord.voice.transport_connect_started",
                guild_id = guild.guild_id,
                playback_generation = guild.source_generation.load(Ordering::Acquire),
                transport_generation = request_generation,
                recovery_attempt = request_generation,
                previous_voice_state,
                next_voice_state = if recovery {
                    "reconnecting"
                } else {
                    "connecting"
                },
                outcome = "started",
                "starting Discord voice transport negotiation"
            );
            if let Some(previous_voice) = previous_voice {
                session.emit(
                    "voice.state_changed",
                    json!({"guildId": guild.guild_id, "previousState": previous_voice_state, "state": "reconnecting"}),
                );
                if let Err(error) = previous_voice.disconnect().await {
                    // Keep output parked during the bounded client-side retry
                    // sequence. A single failed close is not yet a terminal
                    // voice failure; `voice.fail` reports that after retries.
                    tracing::warn!(
                        event = "discord.voice.transport_close_failed",
                        guild_id = guild.guild_id,
                        playback_generation = guild.source_generation.load(Ordering::Acquire),
                        transport_generation = request_generation,
                        recovery_attempt = request_generation,
                        previous_voice_state,
                        next_voice_state = "reconnecting",
                        error = %error,
                        outcome = "retryable_failure",
                        "old voice transport did not close cleanly during replacement"
                    );
                    guild.voice_reconnecting.store(true, Ordering::Release);
                    return Err(RpcError::new(
                        "transport_failed",
                        "could not replace Discord voice transport",
                    ));
                }
            } else if !recovery {
                session.emit(
                    "voice.state_changed",
                    json!({"guildId": guild.guild_id, "previousState": "disconnected", "state": "connecting"}),
                );
            }

            let connection_info = VoiceConnectionInfo {
                guild_id: parse_snowflake(&params.guild_id).expect("validated snowflake"),
                channel_id: parse_snowflake(&params.channel_id).expect("validated snowflake"),
                user_id: guild.bot_user_id,
                session_id: params.session_id,
                token: params.token,
                endpoint: params.endpoint,
            };
            let voice = match DiscordVoiceConnection::connect_with_observability(
                connection_info,
                server.observability.clone(),
            )
            .await
            {
                Ok(voice) => voice,
                Err(error) => {
                    tracing::warn!(
                        event = "discord.voice.transport_connect_failed",
                        guild_id = guild.guild_id,
                        playback_generation = guild.source_generation.load(Ordering::Acquire),
                        transport_generation = request_generation,
                        recovery_attempt = request_generation,
                        previous_voice_state,
                        next_voice_state = if recovery { "reconnecting" } else { "failed" },
                        error = %error,
                        outcome = if recovery { "retryable_failure" } else { "terminal_failure" },
                        "Discord voice transport negotiation failed"
                    );
                    if !recovery
                        && guild.voice_request_generation.load(Ordering::Acquire)
                            == request_generation
                    {
                        guild.voice_reconnecting.store(false, Ordering::Release);
                        session.emit(
                            "voice.state_changed",
                            json!({"guildId": guild.guild_id, "previousState": "connecting", "state": "failed"}),
                        );
                        session.emit(
                            "voice.failed",
                            json!({"guildId": guild.guild_id, "state": "failed", "code": "transport_failed"}),
                        );
                    }
                    return Err(RpcError::new(
                        "transport_failed",
                        "Discord voice connection failed",
                    ));
                }
            };
            if guild.closed.load(Ordering::Acquire)
                || session.closed.load(Ordering::Acquire)
                || guild.voice_request_generation.load(Ordering::Acquire) != request_generation
            {
                let _ = voice.disconnect().await;
                return Err(RpcError::new(
                    if guild.closed.load(Ordering::Acquire)
                        || session.closed.load(Ordering::Acquire)
                    {
                        "unknown_session"
                    } else {
                        "invalid_state"
                    },
                    if guild.closed.load(Ordering::Acquire)
                        || session.closed.load(Ordering::Acquire)
                    {
                        "guild session was closed during voice connection"
                    } else {
                        "Discord voice connection attempt was superseded"
                    },
                ));
            }
            let initial_state = voice_state_name(voice.state());
            guild.voice_sender_router.attach(voice.opus_sender());
            *lock(&guild.voice) = Some(voice);
            guild
                .voice_connected_generation
                .store(request_generation, Ordering::Release);
            guild.voice_reconnecting.store(false, Ordering::Release);
            tracing::info!(
                event = "discord.voice.transport_connect_finished",
                guild_id = guild.guild_id,
                playback_generation = guild.source_generation.load(Ordering::Acquire),
                transport_generation = request_generation,
                recovery_attempt = request_generation,
                previous_voice_state,
                next_voice_state = initial_state,
                outcome = "connected",
                "Discord voice transport and DAVE negotiation completed"
            );
            session.emit(
                "voice.state_changed",
                json!({"guildId": guild.guild_id, "previousState": if recovery { "reconnecting" } else { "connecting" }, "state": initial_state}),
            );
            spawn_voice_monitor(
                Arc::clone(&guild),
                Arc::downgrade(&session),
                voice_generation,
                request_generation,
            );
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
            let state = if guild.voice_reconnecting.load(Ordering::Acquire) {
                "reconnecting"
            } else {
                voice
                    .as_ref()
                    .map(|voice| voice_state_name(voice.state()))
                    .unwrap_or("disconnected")
            };
            Ok(json!({
                "state": state,
                "failed": voice.as_ref().is_some_and(|voice| voice.failure().is_some()),
            }))
        }
        "voice.suspend" => {
            let params: VoiceOperationParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let request_generation = claim_voice_generation(
                &guild.voice_request_generation,
                params.transport_generation,
            )?;
            if guild.voice_terminal_generation.load(Ordering::Acquire) >= request_generation {
                return Err(RpcError::new(
                    "invalid_state",
                    "Discord voice recovery was cancelled",
                ));
            }
            if guild.voice_connected_generation.load(Ordering::Acquire) == request_generation
                && lock(&guild.voice).is_some()
            {
                return Ok(
                    json!({"guildId": guild.guild_id, "state": "connected", "alreadyConnected": true}),
                );
            }
            let _generation = guild.voice_generation.fetch_add(1, Ordering::AcqRel) + 1;
            guild.voice_reconnecting.store(true, Ordering::Release);
            guild.voice_sender_router.detach();
            let _voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            if guild.voice_request_generation.load(Ordering::Acquire) != request_generation {
                return Ok(
                    json!({"guildId": guild.guild_id, "state": "reconnecting", "superseded": true}),
                );
            }
            let voice = { lock(&guild.voice).take() };
            if let Some(voice) = voice {
                let previous_state = voice_state_name(voice.state());
                tracing::info!(
                    event = "discord.voice.transport_suspended",
                    guild_id = guild.guild_id,
                    playback_generation = guild.source_generation.load(Ordering::Acquire),
                    transport_generation = request_generation,
                    previous_voice_state = previous_state,
                    next_voice_state = "reconnecting",
                    outcome = "parked",
                    "parked playback while replacing Discord voice transport"
                );
                voice.disconnect().await.map_err(|error| {
                    tracing::warn!(
                        event = "discord.voice.transport_suspend_failed",
                        guild_id = guild.guild_id,
                        playback_generation = guild.source_generation.load(Ordering::Acquire),
                        transport_generation = request_generation,
                        previous_voice_state = previous_state,
                        next_voice_state = "reconnecting",
                        error = %error,
                        outcome = "retryable_failure",
                        "old voice transport did not suspend cleanly"
                    );
                    RpcError::new(
                        "transport_failed",
                        "could not suspend Discord voice transport",
                    )
                })?;
                session.emit(
                    "voice.state_changed",
                    json!({"guildId": guild.guild_id, "previousState": previous_state, "state": "reconnecting"}),
                );
            }
            Ok(json!({"guildId": guild.guild_id, "state": "reconnecting"}))
        }
        "voice.disconnect" => {
            let params: VoiceOperationParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let request_generation = claim_voice_generation(
                &guild.voice_request_generation,
                params.transport_generation,
            )?;
            let _voice_generation = guild.voice_generation.fetch_add(1, Ordering::AcqRel) + 1;
            guild
                .voice_terminal_generation
                .fetch_max(request_generation, Ordering::AcqRel);
            guild.voice_reconnecting.store(false, Ordering::Release);
            guild.voice_sender_router.detach();
            let voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            if guild.voice_request_generation.load(Ordering::Acquire) != request_generation {
                return Ok(
                    json!({"guildId": guild.guild_id, "state": "reconnecting", "superseded": true}),
                );
            }
            stop_playback(&guild).await?;
            let voice = { lock(&guild.voice).take() };
            let previous_voice_state = voice
                .as_ref()
                .map(|voice| voice_state_name(voice.state()))
                .unwrap_or("disconnected");
            if previous_voice_state != "disconnected" {
                session.emit(
                    "voice.state_changed",
                    json!({"guildId": guild.guild_id, "previousState": previous_voice_state, "state": "disconnecting"}),
                );
            }
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
                            json!({"guildId": guild.guild_id, "state": "failed", "code": "transport_failed"}),
                        );
                        RpcError::new("transport_failed", "Discord voice disconnection failed")
                    })?;
            }
            guild
                .voice_sender_router
                .shutdown("Discord voice session was disconnected");
            if previous_voice_state != "disconnected" {
                session.emit(
                    "voice.state_changed",
                    json!({"guildId": guild.guild_id, "previousState": "disconnecting", "state": "disconnected"}),
                );
            }
            Ok(json!({ "guildId": guild.guild_id, "state": "disconnected" }))
        }
        "voice.fail" => {
            let params: VoiceOperationParams = decode_params(request.params)?;
            validate_snowflake(&params.guild_id, "guildId")?;
            let session = require_client(&connection)?;
            let guild = server
                .get_guild(&session, &params.guild_id)
                .map_err(state_error)?;
            let request_generation = claim_voice_generation(
                &guild.voice_request_generation,
                params.transport_generation,
            )?;
            if guild.voice_terminal_generation.load(Ordering::Acquire) >= request_generation {
                return Ok(
                    json!({"guildId": guild.guild_id, "state": "failed", "alreadyFailed": true}),
                );
            }
            let _voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
            guild.ensure_open().map_err(state_error)?;
            if guild.voice_request_generation.load(Ordering::Acquire) != request_generation {
                return Ok(
                    json!({"guildId": guild.guild_id, "state": "reconnecting", "superseded": true}),
                );
            }
            let _voice_generation = guild.voice_generation.fetch_add(1, Ordering::AcqRel) + 1;
            guild
                .voice_terminal_generation
                .fetch_max(request_generation, Ordering::AcqRel);
            guild.voice_reconnecting.store(false, Ordering::Release);
            guild
                .voice_sender_router
                .fail("Discord voice recovery failed");
            tracing::error!(
                event = "discord.voice.recovery_exhausted",
                guild_id = guild.guild_id,
                playback_generation = guild.source_generation.load(Ordering::Acquire),
                transport_generation = request_generation,
                recovery_attempt = request_generation,
                previous_voice_state = "reconnecting",
                next_voice_state = "failed",
                outcome = "terminal_failure",
                "Discord voice recovery is no longer possible"
            );
            let voice = { lock(&guild.voice).take() };
            if let Some(voice) = voice {
                let _ = voice.disconnect().await;
            }
            session.emit(
                "voice.state_changed",
                json!({"guildId": guild.guild_id, "previousState": "reconnecting", "state": "failed"}),
            );
            session.emit(
                "voice.failed",
                json!({"guildId": guild.guild_id, "state": "failed", "code": "transport_failed"}),
            );
            Ok(json!({"guildId": guild.guild_id, "state": "failed"}))
        }
        _ => Err(RpcError::new("invalid_request", "unknown voice command")),
    }
}
