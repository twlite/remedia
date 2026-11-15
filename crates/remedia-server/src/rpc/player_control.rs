use super::*;

pub(super) async fn execute_player_control(
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

    // Cancelling a seek must work while that seek is still waiting, so it never queues behind
    // the guild's operation lock.
    if request.command == "player.seek.cancel" {
        decode_params::<GuildParams>(request.params)?;
        guild.ensure_open().map_err(state_error)?;
        let cancelled = guild.playback.cancel_seek();

        return Ok(json!({"cancelled": cancelled}));
    }

    if request.command == "player.stop" {
        if let Some(cancellation) = lock(&guild.play_cancellation).as_ref() {
            cancellation.cancel();
        }

        guild.cancel_pending_stage();
    }
    let operation = Arc::clone(&guild.playback_operations).lock_owned().await;
    guild.ensure_open().map_err(state_error)?;
    match request.command.as_str() {
        "player.pause" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            let previous_state = playback.state();
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.pause()
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            let state = guild.playback.state();
            if previous_state != PlaybackState::Paused && state == PlaybackState::Paused {
                session.emit(
                    "player.paused",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": guild.source_generation.load(Ordering::Acquire),
                        "previousState": playback_state_name(previous_state),
                        "state": "paused",
                    }),
                );
            }
            Ok(json!({"state": playback_state_name(guild.playback.state())}))
        }
        "player.resume" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            let previous_state = playback.state();
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.resume()
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            let state = guild.playback.state();
            if previous_state == PlaybackState::Paused && state == PlaybackState::Playing {
                session.emit(
                    "player.resumed",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": guild.source_generation.load(Ordering::Acquire),
                        "previousState": "paused",
                        "state": "playing",
                    }),
                );
            }
            Ok(json!({"state": playback_state_name(guild.playback.state())}))
        }
        "player.stop" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            let previous_state = playback.state();
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.stop()
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;
            lock(&guild.play_cancellation).take();
            let generation = guild.source_generation.load(Ordering::Acquire);
            if !matches!(
                previous_state,
                PlaybackState::Stopped | PlaybackState::Completed | PlaybackState::Failed
            ) && guild.playback.state() == PlaybackState::Stopped
                && guild.claim_terminal_event(generation)
            {
                session.emit(
                    "player.stopped",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": generation,
                        "previousState": playback_state_name(previous_state),
                        "state": "stopped",
                        "reason": "stop",
                    }),
                );
            }
            Ok(json!({"state": playback_state_name(guild.playback.state())}))
        }
        "player.seek" => {
            let params: SeekParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            let playback = Arc::clone(&guild.playback);
            let position = Duration::from_millis(params.position_ms);
            let previous_position_ms = playback.position().media.map(|p| p.position.as_millis());
            // A seek can take seconds when it is served by a replacement source, and a newer
            // seek, a pause, a volume change or a stop must reach the guild meanwhile. So the
            // operation lock covers submission only; the seek then waits on its own.
            let pending = tokio::task::spawn_blocking(move || {
                let pending = playback.start_seek(position);
                drop(operation);
                pending
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
            .map_err(playback_error)?;

            // If this request ends before the seek does (the deadline passes, the connection
            // drops), the seek is cancelled when it still can be, so it cannot take effect
            // after the client was told it failed. A seek that is already being applied cannot
            // be, and announces itself with `player.seeked` when it finishes.
            let mut abandon = AbandonSeek(Some(pending.canceller()));
            let events = Arc::clone(&session);
            let waiting_guild = Arc::clone(&guild);
            let completion = spawn_observed(
                async move {
                    let outcome = tokio::task::spawn_blocking(move || pending.wait_outcome())
                        .await
                        .map_err(|_| RpcError::new("playback_failed", "playback worker failed"))?
                        .map_err(playback_error)?;
                    let applied_ms = outcome.applied.as_millis();
                    if waiting_guild.playback.crossfade_config().is_none()
                        && previous_position_ms != Some(applied_ms)
                    {
                        events.emit(
                            "player.seeked",
                            json!({
                                "guildId": waiting_guild.guild_id,
                                "generation": waiting_guild.source_generation.load(Ordering::Acquire),
                                "positionMs": applied_ms,
                                "requestedPositionMs": outcome.requested.as_millis(),
                                "previousPositionMs": previous_position_ms,
                                "fallback": outcome.replaced_source,
                            }),
                        );
                    }

                    Ok::<_, RpcError>(outcome)
                },
                &server.observability,
            );
            let outcome = completion
                .await
                .map_err(|_| RpcError::new("playback_failed", "seek task failed"))??;
            abandon.0 = None;

            let mut result = playback_snapshot(
                &guild.playback,
                guild.source_generation.load(Ordering::Acquire),
            )["position"]
                .clone();
            result["requestedPositionMs"] = json!(outcome.requested.as_millis());
            result["appliedPositionMs"] = json!(outcome.applied.as_millis());
            result["fallback"] = json!(outcome.replaced_source);

            Ok(result)
        }
        "player.filter.set" => {
            let params: FilterSetParams = decode_params(request.params)?;
            ensure_param_guild(&params.guild_id, &guild_id)?;
            let playback = Arc::clone(&guild.playback);
            let graph = params.graph;
            let previous_graph = playback.filter_graph();
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.set_filter_graph(Some(&graph))
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "filtergraph worker failed"))?
            .map_err(playback_error)?;
            let graph = guild.playback.filter_graph();
            if previous_graph != graph {
                session.emit(
                    "player.filter_changed",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": guild.source_generation.load(Ordering::Acquire),
                        "previousGraph": previous_graph,
                        "graph": graph,
                    }),
                );
            }
            Ok(json!({"filterGraph": guild.playback.filter_graph()}))
        }
        "player.filter.clear" => {
            decode_params::<GuildParams>(request.params)?;
            let playback = Arc::clone(&guild.playback);
            let previous_graph = playback.filter_graph();
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.set_filter_graph(None)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "filtergraph worker failed"))?
            .map_err(playback_error)?;
            if previous_graph.is_some() {
                session.emit(
                    "player.filter_changed",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": guild.source_generation.load(Ordering::Acquire),
                        "previousGraph": previous_graph,
                        "graph": null,
                    }),
                );
            }
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
            let previous_volume = playback.volume();
            let gain = params.gain as f32;
            tokio::task::spawn_blocking(move || {
                let _operation = operation;
                playback.set_volume(gain)
            })
            .await
            .map_err(|_| RpcError::new("playback_failed", "volume worker failed"))?
            .map_err(playback_error)?;
            if f64::from(previous_volume) != params.gain {
                session.emit(
                    "player.volume_changed",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": guild.source_generation.load(Ordering::Acquire),
                        "previousVolume": previous_volume,
                        "volume": params.gain,
                        "gain": params.gain,
                    }),
                );
            }
            Ok(json!({"volume": guild.playback.volume()}))
        }
        _ => Err(RpcError::new("invalid_request", "unknown player command")),
    }
}

pub(super) async fn stop_playback(guild: &Arc<GuildSession>) -> Result<(), RpcError> {
    if let Some(cancellation) = lock(&guild.play_cancellation).as_ref() {
        cancellation.cancel();
    }
    guild.cancel_pending_stage();
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

/// Cancels a seek that no request is waiting for any more.
struct AbandonSeek(Option<remedia_core::SeekCanceller>);

impl Drop for AbandonSeek {
    fn drop(&mut self) {
        if let Some(canceller) = self.0.take() {
            canceller.cancel();
        }
    }
}
