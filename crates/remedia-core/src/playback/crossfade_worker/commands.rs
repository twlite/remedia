//! Pause, seek, filter, and shutdown commands for continuous PCM playback.

use super::*;

/// Respond to the existing pause/resume/seek/filter command protocol. This
/// logic is deliberately separate from the legacy worker.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_commands(
    shared: &SessionShared,
    generation: u64,
    output: &Arc<dyn MediaOutput>,
    cancellation: &CancellationToken,
    current: &mut ActiveSource,
    processor: &mut PcmProcessor,
    transition: &mut Option<Transition>,
    stage_slot: &Mutex<Option<Arc<PreparedStage>>>,
    preparation_requested: &mut bool,
) -> Result<(), PlaybackError> {
    loop {
        let mut data = lock(&shared.data);
        if data.generation != generation || data.stopping {
            return Err(PlaybackError::Cancelled);
        }
        if let Some(request) = data.pending_seek.take() {
            drop(data);
            if let Some(fade) = transition.take() {
                fade.next.cancel_and_join();
                {
                    let mut state = lock(&shared.data);
                    state.source_generation = fade.previous_generation;
                    state.info = Some(fade.outgoing_info.clone());
                    state.duration = fade.outgoing_info.duration;
                }
                shared.publish(CrossfadeEvent::TransitionCancelled {
                    outgoing_generation: fade.previous_generation,
                    incoming_generation: fade.incoming_generation,
                    identity: fade.identity.clone(),
                });
            }
            if let Some(stage) = lock(stage_slot).take() {
                shared.publish(CrossfadeEvent::StageInvalidated {
                    identity: stage.identity.clone(),
                });
                stage.cancel_and_join();
            }
            let result = current.seek(request.position, cancellation).and_then(|()| {
                let sample = samples_from_duration(request.position);
                processor
                    .reset(sample)
                    .map_err(|e| PlaybackError::FilterGraph(Arc::new(e)))?;
                output.flush_samples(sample, cancellation)?;
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
                shared.publish(CrossfadeEvent::TimelineChanged {
                    previous_generation,
                    generation,
                    position_ms: request.position.as_millis() as u64,
                });
                Ok(())
            });
            let _ = request.response.send(result.clone());
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
            let result = output
                .filter_transition(processor.next_sample(), cancellation)
                .map_err(PlaybackError::from);
            if result.is_ok() {
                processor.replace(update.graph);
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
                    shared.changed.notify_all();
                }
            }
            PlaybackState::Resuming => {
                drop(data);
                output.resume(cancellation)?;
                let mut state = lock(&shared.data);
                if state.state == PlaybackState::Resuming {
                    state.state = PlaybackState::Playing;
                    shared.changed.notify_all();
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
