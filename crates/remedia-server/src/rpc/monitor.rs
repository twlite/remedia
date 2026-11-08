use super::*;

pub(super) fn spawn_playback_monitor(
    guild: Arc<GuildSession>,
    session: std::sync::Weak<ClientSession>,
    generation: u64,
    core_generation: u64,
) {
    // Crossfade sessions keep one output worker alive across source promotions.
    // Poll its ordered native event queue instead of waiting for the first
    // source generation to terminate. The legacy waiter remains below.
    if guild.playback.crossfade_config().is_some() {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(25));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut logical_to_rpc = std::collections::HashMap::new();
            logical_to_rpc.insert(guild.playback.source_generation(), generation);
            let mut active_rpc = generation;
            loop {
                interval.tick().await;
                if guild.closed.load(Ordering::Acquire)
                    || guild.playback.completion_generation() != core_generation
                {
                    return;
                }
                let Some(session) = session.upgrade() else { return };
                for event in guild.playback.take_crossfade_events() {
                    match event {
                        remedia_core::CrossfadeEvent::PrepareRequested {
                            generation: source_generation,
                            remaining_ms,
                        } => {
                            let rpc_gen = logical_to_rpc.get(&source_generation).copied()
                                .unwrap_or(active_rpc);
                            let fade_ms = guild.playback.crossfade_config()
                                .map(|config| config.duration_ms).unwrap_or(0);
                            session.emit("player.prepare_requested", json!({
                                "guildId": guild.guild_id,
                                "generation": rpc_gen,
                                "remainingMs": remaining_ms,
                                "transitionInMs": remaining_ms.saturating_sub(u64::from(fade_ms)),
                            }));
                        }
                        remedia_core::CrossfadeEvent::StageReady { identity } => {
                            session.emit("player.stage_ready", json!({
                                "guildId": guild.guild_id,
                                "generation": active_rpc,
                                "entryId": identity.entry_id,
                                "queueRevision": identity.queue_revision,
                                "sourceRevision": identity.source_revision,
                            }));
                        }
                        remedia_core::CrossfadeEvent::StageFailed { identity, reason } => {
                            session.emit("player.stage_failed", json!({
                                "guildId": guild.guild_id,
                                "generation": active_rpc,
                                "entryId": identity.entry_id,
                                "queueRevision": identity.queue_revision,
                                "sourceRevision": identity.source_revision,
                                "reason": reason,
                            }));
                        }
                        remedia_core::CrossfadeEvent::StageInvalidated { identity } => {
                            session.emit("player.stage_invalidated", json!({
                                "guildId": guild.guild_id,
                                "generation": active_rpc,
                                "entryId": identity.entry_id,
                                "queueRevision": identity.queue_revision,
                                "sourceRevision": identity.source_revision,
                            }));
                        }
                        remedia_core::CrossfadeEvent::TransitionStarted {
                            outgoing_generation,
                            incoming_generation,
                            identity,
                            info,
                        } => {
                            let previous = logical_to_rpc.get(&outgoing_generation).copied()
                                .unwrap_or(active_rpc);
                            let next = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
                            logical_to_rpc.insert(incoming_generation, next);
                            active_rpc = next;
                            session.emit("player.started", json!({
                                "guildId": guild.guild_id,
                                "generation": next,
                                "source": stream_info_json(info),
                                "transition": {
                                    "type": "crossfade",
                                    "outgoingGeneration": previous,
                                    "entryId": identity.entry_id,
                                },
                            }));
                        }
                        remedia_core::CrossfadeEvent::TransitionCompleted {
                            outgoing_generation,
                            incoming_generation,
                            identity,
                        } => {
                            let previous = logical_to_rpc.get(&outgoing_generation).copied()
                                .unwrap_or(generation);
                            let next = logical_to_rpc.get(&incoming_generation).copied()
                                .unwrap_or(active_rpc);
                            if guild.claim_terminal_event(previous) {
                                session.emit("player.completed", json!({
                                    "guildId": guild.guild_id,
                                    "generation": previous,
                                    "state": "completed",
                                    "continuation": {
                                        "type": "promoted",
                                        "generation": next,
                                        "entryId": identity.entry_id,
                                    },
                                }));
                            }
                        }
                        remedia_core::CrossfadeEvent::TransitionCancelled {
                            outgoing_generation,
                            incoming_generation,
                            identity,
                        } => {
                            let outgoing = logical_to_rpc.get(&outgoing_generation).copied()
                                .unwrap_or(generation);
                            let failed = logical_to_rpc.get(&incoming_generation).copied()
                                .unwrap_or(active_rpc);
                            let resumed = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
                            logical_to_rpc.insert(outgoing_generation, resumed);
                            active_rpc = resumed;
                            session.emit("player.transition_cancelled", json!({
                                "guildId": guild.guild_id,
                                "generation": resumed,
                                "outgoingGeneration": outgoing,
                                "failedGeneration": failed,
                                "entryId": identity.entry_id,
                            }));
                        }
                    }
                }
                let state = guild.playback.state();
                if matches!(state, PlaybackState::Completed | PlaybackState::Failed | PlaybackState::Stopped) {
                    if guild.claim_terminal_event(active_rpc) {
                        match state {
                            PlaybackState::Completed => session.emit("player.completed", json!({
                                "guildId": guild.guild_id,
                                "generation": active_rpc,
                                "state": "completed",
                            })),
                            PlaybackState::Failed => session.emit("player.failed", json!({
                                "guildId": guild.guild_id,
                                "generation": active_rpc,
                                "state": "failed",
                                "code": "playback_failed",
                            })),
                            PlaybackState::Stopped => session.emit("player.stopped", json!({
                                "guildId": guild.guild_id,
                                "generation": active_rpc,
                                "state": "stopped",
                                "reason": "stopped",
                            })),
                            _ => {}
                        }
                    }
                    lock(&guild.play_cancellation).take();
                    return;
                }
            }
        });
        return;
    }
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
