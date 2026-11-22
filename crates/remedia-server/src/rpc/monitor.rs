use super::*;

/// Keep only the generations that current queued lifecycle events can still reference.
/// An overflow discards this inferred history and starts again from the session snapshot.
struct RpcGenerationTracker {
    active_core: u64,
    active_rpc: u64,
    previous: Option<(u64, u64)>,
    transition: Option<(u64, u64)>,
}

impl RpcGenerationTracker {
    fn new(core_generation: u64, rpc_generation: u64) -> Self {
        Self {
            active_core: core_generation,
            active_rpc: rpc_generation,
            previous: None,
            transition: None,
        }
    }

    fn active_rpc(&self) -> u64 {
        self.active_rpc
    }

    fn is_active(&self, core_generation: u64) -> bool {
        self.active_core == core_generation
    }

    fn rpc_for(&self, core_generation: u64) -> Option<u64> {
        if self.active_core == core_generation {
            return Some(self.active_rpc);
        }

        self.previous
            .filter(|(previous, _)| *previous == core_generation)
            .map(|(_, rpc)| rpc)
    }

    fn timeline_changed(
        &mut self,
        previous_core: u64,
        next_core: u64,
        next_rpc: u64,
    ) -> Option<u64> {
        if self.active_core != previous_core || previous_core == next_core {
            return None;
        }

        let previous_rpc = self.active_rpc;
        self.previous = Some((previous_core, previous_rpc));
        self.active_core = next_core;
        self.active_rpc = next_rpc;
        self.transition = None;

        Some(previous_rpc)
    }

    fn transition_started(
        &mut self,
        outgoing_core: u64,
        incoming_core: u64,
        incoming_rpc: u64,
    ) -> Option<u64> {
        if self.active_core != outgoing_core || outgoing_core == incoming_core {
            return None;
        }

        let outgoing_rpc = self.active_rpc;
        self.previous = Some((outgoing_core, outgoing_rpc));
        self.active_core = incoming_core;
        self.active_rpc = incoming_rpc;
        self.transition = Some((outgoing_core, incoming_core));

        Some(outgoing_rpc)
    }

    fn transition_completed(
        &mut self,
        outgoing_core: u64,
        incoming_core: u64,
    ) -> Option<(u64, u64)> {
        if self.transition != Some((outgoing_core, incoming_core))
            || self.active_core != incoming_core
        {
            return None;
        }

        let outgoing_rpc = self.rpc_for(outgoing_core)?;
        let incoming_rpc = self.active_rpc;
        self.previous = None;
        self.transition = None;

        Some((outgoing_rpc, incoming_rpc))
    }

    fn transition_cancelled(
        &mut self,
        outgoing_core: u64,
        incoming_core: u64,
        resumed_rpc: u64,
    ) -> Option<(u64, u64)> {
        if self.transition != Some((outgoing_core, incoming_core))
            || self.active_core != incoming_core
        {
            return None;
        }

        let outgoing_rpc = self.rpc_for(outgoing_core)?;
        let failed_rpc = self.active_rpc;
        self.active_core = outgoing_core;
        self.active_rpc = resumed_rpc;
        self.previous = None;
        self.transition = None;

        Some((outgoing_rpc, failed_rpc))
    }

    fn resynchronize(&mut self, core_generation: u64, rpc_generation: u64) {
        self.active_core = core_generation;
        self.active_rpc = rpc_generation;
        self.previous = None;
        self.transition = None;
    }

    fn has_transition(&self, outgoing_core: u64, incoming_core: u64) -> bool {
        self.transition == Some((outgoing_core, incoming_core)) && self.active_core == incoming_core
    }

    #[cfg(test)]
    fn retained_generations(&self) -> usize {
        usize::from(self.previous.is_some()) + 1
    }
}

pub(super) fn spawn_playback_monitor(
    guild: Arc<GuildSession>,
    session: std::sync::Weak<ClientSession>,
    generation: u64,
    core_generation: u64,
) {
    if guild.playback.crossfade_config().is_some() {
        spawn_crossfade_monitor(guild, session, generation, core_generation);
        return;
    }
    let waiter = guild.playback.completion_for_generation(core_generation);
    tokio::spawn(async move {
        let state = waiter.wait_async().await;
        if guild.closed.load(Ordering::Acquire) {
            return;
        }
        if guild.source_generation.load(Ordering::Acquire) != generation {
            return;
        }
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

fn spawn_crossfade_monitor(
    guild: Arc<GuildSession>,
    session: std::sync::Weak<ClientSession>,
    generation: u64,
    core_generation: u64,
) {
    tokio::spawn(async move {
        let mut revision = guild.playback.crossfade_events_revision();
        let mut generations =
            RpcGenerationTracker::new(guild.playback.source_generation(), generation);

        loop {
            if guild.closed.load(Ordering::Acquire)
                || guild.playback.completion_generation() != core_generation
            {
                return;
            }

            let events = guild.playback.take_crossfade_events();
            let Some(session) = session.upgrade() else {
                return;
            };

            let dropped = events.iter().find_map(|event| match event {
                remedia_core::CrossfadeEvent::EventsDropped { count } => Some(*count),
                _ => None,
            });

            if let Some(dropped_events) = dropped {
                let core_generation = guild.playback.source_generation();
                let next = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
                generations.resynchronize(core_generation, next);
                let omitted_events =
                    dropped_events.saturating_add(events.len().saturating_sub(1) as u64);
                session.emit(
                    "player.resynchronized",
                    json!({
                        "guildId": guild.guild_id,
                        "generation": next,
                        "droppedEvents": omitted_events,
                        "snapshot": playback_snapshot(&guild.playback, next),
                    }),
                );
            } else {
                for event in events {
                    match event {
                        remedia_core::CrossfadeEvent::EventsDropped { .. } => unreachable!(),
                        remedia_core::CrossfadeEvent::TimelineChanged {
                            previous_generation,
                            generation: logical,
                            position_ms,
                            requested_position_ms,
                            replaced_source,
                        } => {
                            if !generations.is_active(previous_generation)
                                || previous_generation == logical
                            {
                                continue;
                            }

                            let next = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
                            let Some(previous) =
                                generations.timeline_changed(previous_generation, logical, next)
                            else {
                                continue;
                            };

                            session.emit(
                                "player.seeked",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": next,
                                    "previousGeneration": previous,
                                    "positionMs": position_ms,
                                    "requestedPositionMs": requested_position_ms,
                                    "fallback": replaced_source,
                                }),
                            );
                        }
                        remedia_core::CrossfadeEvent::PrepareRequested {
                            generation: source_generation,
                            remaining_ms,
                        } => {
                            let Some(rpc_generation) = generations.rpc_for(source_generation)
                            else {
                                continue;
                            };
                            let fade_ms = guild
                                .playback
                                .crossfade_config()
                                .map(|config| config.duration_ms)
                                .unwrap_or(0);

                            session.emit(
                                "player.prepare_requested",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": rpc_generation,
                                    "remainingMs": remaining_ms,
                                    "transitionInMs": remaining_ms.saturating_sub(u64::from(fade_ms)),
                                }),
                            );
                        }
                        remedia_core::CrossfadeEvent::StageReady { identity } => {
                            let Some(rpc_generation) =
                                generations.rpc_for(identity.expected_generation)
                            else {
                                continue;
                            };

                            session.emit(
                                "player.stage_ready",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": rpc_generation,
                                    "entryId": identity.entry_id,
                                    "queueRevision": identity.queue_revision,
                                    "sourceRevision": identity.source_revision,
                                }),
                            );
                        }
                        remedia_core::CrossfadeEvent::StageFailed { identity, reason } => {
                            let Some(rpc_generation) =
                                generations.rpc_for(identity.expected_generation)
                            else {
                                continue;
                            };

                            session.emit(
                                "player.stage_failed",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": rpc_generation,
                                    "entryId": identity.entry_id,
                                    "queueRevision": identity.queue_revision,
                                    "sourceRevision": identity.source_revision,
                                    "reason": reason,
                                }),
                            );
                        }
                        remedia_core::CrossfadeEvent::StageInvalidated { identity } => {
                            let Some(rpc_generation) =
                                generations.rpc_for(identity.expected_generation)
                            else {
                                continue;
                            };

                            session.emit(
                                "player.stage_invalidated",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": rpc_generation,
                                    "entryId": identity.entry_id,
                                    "queueRevision": identity.queue_revision,
                                    "sourceRevision": identity.source_revision,
                                }),
                            );
                        }
                        remedia_core::CrossfadeEvent::TransitionStarted {
                            outgoing_generation,
                            incoming_generation,
                            identity,
                            info,
                        } => {
                            if !generations.is_active(outgoing_generation)
                                || outgoing_generation == incoming_generation
                            {
                                continue;
                            }

                            let next = guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
                            let Some(previous) = generations.transition_started(
                                outgoing_generation,
                                incoming_generation,
                                next,
                            ) else {
                                continue;
                            };

                            session.emit(
                                "player.started",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": next,
                                    "source": stream_info_json(info),
                                    "transition": {
                                        "type": "crossfade",
                                        "outgoingGeneration": previous,
                                        "entryId": identity.entry_id,
                                    },
                                }),
                            );
                        }
                        remedia_core::CrossfadeEvent::TransitionCompleted {
                            outgoing_generation,
                            incoming_generation,
                            identity,
                        } => {
                            let Some((previous, next)) = generations
                                .transition_completed(outgoing_generation, incoming_generation)
                            else {
                                continue;
                            };

                            if guild.claim_terminal_event(previous) {
                                session.emit(
                                    "player.completed",
                                    json!({
                                        "guildId": guild.guild_id,
                                        "generation": previous,
                                        "state": "completed",
                                        "continuation": {
                                            "type": "promoted",
                                            "generation": next,
                                            "entryId": identity.entry_id,
                                        },
                                    }),
                                );
                            }
                        }
                        remedia_core::CrossfadeEvent::TransitionCancelled {
                            outgoing_generation,
                            incoming_generation,
                            identity,
                        } => {
                            if !generations.has_transition(outgoing_generation, incoming_generation)
                            {
                                continue;
                            }

                            let resumed_rpc =
                                guild.source_generation.fetch_add(1, Ordering::AcqRel) + 1;
                            let Some((outgoing, failed)) = generations.transition_cancelled(
                                outgoing_generation,
                                incoming_generation,
                                resumed_rpc,
                            ) else {
                                continue;
                            };

                            session.emit(
                                "player.transition_cancelled",
                                json!({
                                    "guildId": guild.guild_id,
                                    "generation": resumed_rpc,
                                    "outgoingGeneration": outgoing,
                                    "failedGeneration": failed,
                                    "entryId": identity.entry_id,
                                }),
                            );
                        }
                    }
                }
            }

            let state = guild.playback.state();
            if emit_crossfade_terminal(&guild, &session, state, generations.active_rpc()) {
                lock(&guild.play_cancellation).take();
                return;
            }

            drop(session);
            revision = guild.playback.wait_for_crossfade_events(revision).await;
        }
    });
}

fn emit_crossfade_terminal(
    guild: &GuildSession,
    session: &ClientSession,
    state: PlaybackState,
    generation: u64,
) -> bool {
    if !matches!(
        state,
        PlaybackState::Completed | PlaybackState::Failed | PlaybackState::Stopped
    ) {
        return false;
    }

    if guild.claim_terminal_event(generation) {
        match state {
            PlaybackState::Completed => session.emit(
                "player.completed",
                json!({
                    "guildId": guild.guild_id,
                    "generation": generation,
                    "state": "completed",
                }),
            ),
            PlaybackState::Failed => session.emit(
                "player.failed",
                json!({
                    "guildId": guild.guild_id,
                    "generation": generation,
                    "state": "failed",
                    "code": "playback_failed",
                }),
            ),
            PlaybackState::Stopped => session.emit(
                "player.stopped",
                json!({
                    "guildId": guild.guild_id,
                    "generation": generation,
                    "state": "stopped",
                    "reason": "stopped",
                }),
            ),
            _ => unreachable!(),
        }
    }

    true
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

#[cfg(test)]
mod tests {
    use super::RpcGenerationTracker;

    #[test]
    fn generation_tracker_prunes_seek_fallback_and_cancelled_transition_history() {
        let mut generations = RpcGenerationTracker::new(1, 50);

        assert_eq!(generations.timeline_changed(1, 2, 51), Some(50));
        assert_eq!(generations.rpc_for(1), Some(50));
        assert_eq!(generations.rpc_for(2), Some(51));

        assert_eq!(generations.transition_started(2, 3, 52), Some(51));
        assert_eq!(generations.rpc_for(2), Some(51));
        assert_eq!(generations.rpc_for(3), Some(52));
        assert_eq!(generations.retained_generations(), 2);
        assert_eq!(generations.transition_cancelled(2, 3, 53), Some((51, 52)));
        assert_eq!(generations.active_rpc(), 53);
        assert_eq!(generations.rpc_for(2), Some(53));
        assert_eq!(generations.rpc_for(3), None);
        assert_eq!(generations.retained_generations(), 1);

        assert_eq!(generations.timeline_changed(2, 4, 54), Some(53));
        assert_eq!(generations.rpc_for(2), Some(53));
        assert_eq!(generations.rpc_for(4), Some(54));
        assert_eq!(generations.rpc_for(1), None);

        generations.resynchronize(9, 70);
        assert_eq!(generations.rpc_for(9), Some(70));
        assert_eq!(generations.rpc_for(2), None);
        assert_eq!(generations.rpc_for(4), None);
        assert_eq!(generations.retained_generations(), 1);
    }

    #[test]
    fn completed_transition_drops_its_previous_mapping_and_churn_stays_bounded() {
        let mut generations = RpcGenerationTracker::new(10, 100);

        for step in 0..10_000 {
            let outgoing = 10 + step;
            let incoming = outgoing + 1;
            let outgoing_rpc = 100 + step;
            let incoming_rpc = outgoing_rpc + 1;

            assert_eq!(
                generations.transition_started(outgoing, incoming, incoming_rpc),
                Some(outgoing_rpc)
            );
            assert_eq!(generations.retained_generations(), 2);
            assert_eq!(
                generations.transition_completed(outgoing, incoming),
                Some((outgoing_rpc, incoming_rpc))
            );
            assert_eq!(generations.rpc_for(outgoing), None);
            assert_eq!(generations.rpc_for(incoming), Some(incoming_rpc));
            assert_eq!(generations.retained_generations(), 1);
        }
    }
}
