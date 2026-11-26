//! Pause, seek, filter, and shutdown commands for continuous PCM playback.

use super::*;

/// Respond to the existing pause/resume/seek/filter command protocol. This
/// logic is deliberately separate from the legacy worker.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_commands(
    shared: &Arc<SessionShared>,
    generation: u64,
    output: &Arc<dyn MediaOutput>,
    cancellation: &CancellationToken,
    current: &mut ActiveSource,
    processor: &mut PcmProcessor,
    transition: &mut Option<Transition>,
    stage_slot: &Mutex<Option<Arc<PreparedStage>>>,
    preparation_requested: &mut bool,
    fallback: &mut Option<fallback::FallbackSeek>,
) -> Result<(), PlaybackError> {
    loop {
        let mut data = lock(&shared.data);
        if data.generation != generation || data.stopping {
            return Err(PlaybackError::Cancelled);
        }
        if let Some(request) = data.pending_seek.take() {
            let fallback_allowed = data.fallback_seeking && current.reopen().available();
            drop(data);

            // Only the latest seek may replace the active source.
            if let Some(previous) = fallback.take() {
                previous.supersede();
            }

            if request.ticket.is_cancelled() {
                let _ = request.response.send(Err(PlaybackError::SeekCancelled));
                continue;
            }

            // A decoder that cannot reposition itself is never asked to: a failed attempt on an
            // HTTP source can drop the connection and lose the current position.
            let attempt = if fallback_allowed && !current.can_seek_in_place() {
                Err(PlaybackError::UnsupportedSeeking(
                    "the source cannot reposition itself".to_owned(),
                ))
            } else if !request.ticket.commit() {
                // From here the seek changes the source, so it can no longer be cancelled.
                let _ = request.response.send(Err(PlaybackError::SeekCancelled));
                continue;
            } else {
                current.seek(request.position, cancellation)
            };
            if let Err(PlaybackError::UnsupportedSeeking(_)) = &attempt
                && fallback_allowed
            {
                // Nothing is torn down: the current source, its transition and the staged next
                // track stay as they are until the replacement is ready.
                request.ticket.uncommit();
                let origin = current.consumed;
                let expected = lock(&shared.data).info.clone();
                match fallback::FallbackSeek::start(
                    shared,
                    current.reopen(),
                    request,
                    origin,
                    expected,
                    cancellation,
                ) {
                    Ok(started) => {
                        tracing::info!(
                            name: "core.playback.fallback_seek_started",
                            target: "remedia_core::playback",
                            playback_id = lock(&shared.data).playback_id,
                            position_ms = started.target().as_millis() as u64,
                            "seek is preparing a replacement source; playback continues"
                        );
                        *fallback = Some(started);
                    }
                    Err((request, error)) => {
                        let _ = request.response.send(Err(error));
                    }
                }
                continue;
            }

            let result = attempt.and_then(|()| {
                if let Some(fade) = transition.take() {
                    fade.next.cancel_and_join();
                    {
                        let mut state = lock(&shared.data);
                        state.source_generation = fade.previous_generation;
                        state.active_stage = fade.previous_active_stage.clone();
                        state.info = Some(fade.outgoing_info.clone());
                        state.duration = fade.outgoing_info.duration;
                    }
                    shared.publish_crossfade_event(CrossfadeEvent::TransitionCancelled {
                        outgoing_generation: fade.previous_generation,
                        incoming_generation: fade.incoming_generation,
                        identity: fade.identity.clone(),
                    });
                }
                let stage = { lock(stage_slot).take() };

                if let Some(stage) = stage {
                    shared.publish_crossfade_event(CrossfadeEvent::StageInvalidated {
                        identity: stage.identity.clone(),
                    });
                    stage.cancel_and_join();
                }
                let sample = samples_from_duration(request.position);
                processor
                    .reset(sample)
                    .map_err(|e| PlaybackError::FilterGraph(Arc::new(e)))?;
                output.flush_samples(sample, cancellation)?;
                if current.may_resume_encoded()
                    && let Some(encoded) = output.encoded_opus_output()
                {
                    encoded.align_encoder_frames(sample)?;
                    current.mark_encoder_aligned(sample);
                }
                let mut state = lock(&shared.data);
                state.submitted_sample = sample;
                state.submitted_position = request.position;
                state.media_position = Some(MediaPosition {
                    position: request.position,
                    accuracy: MediaPositionAccuracy::Estimated,
                });
                *preparation_requested = false;
                let previous_generation = state.source_generation;
                state.source_serial = state.source_serial.saturating_add(1);
                state.source_generation = state.source_serial;
                state.latest_stage = None;
                let generation = state.source_generation;
                drop(state);
                shared.publish_crossfade_event(CrossfadeEvent::TimelineChanged {
                    previous_generation,
                    generation,
                    position_ms: request.position.as_millis() as u64,
                    requested_position_ms: request.position.as_millis() as u64,
                    replaced_source: false,
                });
                Ok(())
            });
            let _ = request.response.send(result.clone().map(|()| SeekOutcome {
                requested: request.position,
                applied: request.position,
                replaced_source: false,
            }));
            if matches!(
                &result,
                Err(PlaybackError::UnsupportedSeeking(_))
                    | Err(PlaybackError::SeekOutOfRange { .. })
            ) {
                continue;
            }
            result?;
            continue;
        }
        if let Some(update) = data.pending_filter_graph.take() {
            drop(data);
            if current.encoded_mode() {
                if let Err(error) =
                    current.enter_pcm_seamless(output, processor.next_sample(), cancellation)
                {
                    let _ = update.response.send(Err(error.clone()));
                    return Err(error);
                }
                lock(&shared.data).processing_mode = Some(PlaybackProcessingMode::Transcoding);
                shared.signal_change();
            }
            let result = output
                .filter_transition(processor.next_sample(), cancellation)
                .map_err(PlaybackError::from);
            if result.is_ok() {
                processor.replace(update.graph);
                current.forget_encoder_phase();
            }
            let _ = update.response.send(result.clone());
            result?;
            continue;
        }
        match data.state {
            PlaybackState::Pausing => {
                drop(data);
                output.pause(cancellation)?;
                let mut state = lock(&shared.data);
                if state.state == PlaybackState::Pausing {
                    state.state = PlaybackState::Paused;
                    shared.signal_change();
                }
            }
            PlaybackState::Resuming => {
                drop(data);
                output.resume(cancellation)?;
                let mut state = lock(&shared.data);
                if state.state == PlaybackState::Resuming {
                    state.state = PlaybackState::Playing;
                    shared.signal_change();
                }
            }
            PlaybackState::Paused | PlaybackState::Starting => {
                data = wait(&shared.changed, data);
                drop(data);
            }
            PlaybackState::Playing => return Ok(()),
            _ => return Err(PlaybackError::Cancelled),
        }
    }
}
