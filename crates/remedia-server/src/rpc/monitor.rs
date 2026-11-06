use super::*;

pub(super) fn spawn_playback_monitor(
    guild: Arc<GuildSession>,
    session: std::sync::Weak<ClientSession>,
    generation: u64,
    core_generation: u64,
) {
    let waiter = guild.playback.completion_for_generation(core_generation);
    tokio::spawn(async move {
        let state = tokio::task::spawn_blocking(move || waiter.wait()).await;
        if guild.closed.load(Ordering::Acquire) {
            return;
        }
        if guild.source_generation.load(Ordering::Acquire) != generation {
            return;
        }
        let Ok(state) = state else {
            return;
        };
        if guild.claim_terminal_event(generation)
            && let Some(session) = session.upgrade()
        {
            match state {
                PlaybackState::Completed => session.emit(
                    "player.completed",
                    json!({"guildId": guild.guild_id, "generation": generation, "state": "completed"}),
                ),
                PlaybackState::Failed => session.emit(
                    "player.failed",
                    json!({"guildId": guild.guild_id, "generation": generation, "state": "failed", "code": "playback_failed"}),
                ),
                PlaybackState::Stopped => session.emit(
                    "player.stopped",
                    json!({"guildId": guild.guild_id, "generation": generation, "state": "stopped", "reason": "stopped"}),
                ),
                _ => {}
            }
        }
        lock(&guild.play_cancellation).take();
    });
}

pub(super) fn spawn_voice_monitor(
    guild: Arc<GuildSession>,
    session: std::sync::Weak<ClientSession>,
    generation: u64,
    request_generation: u64,
) {
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
            if guild.voice_generation.load(Ordering::Acquire) != generation {
                return;
            }
            if guild.voice_request_generation.load(Ordering::Acquire) != request_generation {
                return;
            }
            let state = lock(&guild.voice)
                .as_ref()
                .map(DiscordVoiceConnection::state);
            if state != previous {
                if let (Some(session), Some(state)) = (session.upgrade(), state) {
                    session.emit(
                        "voice.state_changed",
                        json!({
                            "guildId": guild.guild_id,
                            "previousState": previous.map(voice_state_name).unwrap_or("disconnected"),
                            "state": voice_state_name(state),
                        }),
                    );
                    if state == ConnectionState::Failed {
                        let error = lock(&guild.voice)
                            .as_ref()
                            .and_then(DiscordVoiceConnection::failure)
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "voice transport worker failed".into());
                        tracing::warn!(
                            event = "discord.voice.transport_failed",
                            guild_id = guild.guild_id,
                            transport_generation = request_generation,
                            error = %error,
                            "voice transport stopped; client may request a fresh Gateway handshake"
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
