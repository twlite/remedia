//! Opt-in continuous PCM playback. Disabled sessions retain the original
//! single-source worker and encoded Opus passthrough behavior unchanged.
use super::*;
use crate::crossfade::{CrossfadeWindow, PcmCrossfade};
use std::collections::VecDeque;

const FRAME_SAMPLES: usize = 960;

/// Either the original FFmpeg decoder, or a prebuffered producer that has been
/// promoted without opening another network stream or restarting the encoder.
enum ActiveInput {
    Direct {
        decoder: Box<AudioDecoder>,
        pcm: VecDeque<f32>,
        ended: bool,
    },
    Prepared(Arc<PreparedStage>),
}

struct ActiveSource {
    input: ActiveInput,
    consumed: u64,
    duration_samples: Option<u64>,
}

impl ActiveSource {
    fn decoder(decoder: AudioDecoder) -> Self {
        let duration_samples = decoder.info().duration.map(samples_from_duration);
        Self {
            input: ActiveInput::Direct {
                decoder: Box::new(decoder),
                pcm: VecDeque::new(),
                ended: false,
            },
            consumed: 0,
            duration_samples,
        }
    }

    fn prepared(stage: Arc<PreparedStage>, consumed: u64, info: &AudioStreamInfo) -> Self {
        Self {
            input: ActiveInput::Prepared(stage),
            consumed,
            duration_samples: info.duration.map(samples_from_duration),
        }
    }

    fn read(
        &mut self,
        wanted: usize,
        cancellation: &CancellationToken,
    ) -> Result<Option<Vec<f32>>, PlaybackError> {
        if wanted == 0 {
            return Ok(None);
        }
        let samples = match &mut self.input {
            ActiveInput::Direct {
                decoder,
                pcm,
                ended,
            } => {
                while pcm.len() < wanted * 2 && !*ended && !cancellation.is_cancelled() {
                    match decoder.next_frame()? {
                        Some(frame) => pcm.extend(frame.samples),
                        None => *ended = true,
                    }
                }
                if pcm.is_empty() {
                    return Ok(None);
                }
                pcm.drain(..(wanted * 2).min(pcm.len())).collect()
            }
            ActiveInput::Prepared(stage) => {
                match stage.buffer.read_for_playback(wanted, cancellation) {
                    Some(frame) => frame,
                    None if cancellation.is_cancelled() => return Err(PlaybackError::Cancelled),
                    None if stage.snapshot().state == StageState::Failed => {
                        return Err(PlaybackError::UnsupportedSeeking(
                            "prepared source failed while decoding".into(),
                        ));
                    }
                    None => return Ok(None),
                }
            }
        };
        self.consumed += (samples.len() / 2) as u64;
        Ok(Some(samples))
    }

    fn seek(
        &mut self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), PlaybackError> {
        match &mut self.input {
            ActiveInput::Direct {
                decoder,
                pcm,
                ended,
            } => {
                decoder.seek(position)?;
                pcm.clear();
                *ended = false;
            }
            ActiveInput::Prepared(stage) => {
                let source = stage.source.clone();
                if !source.can_seek() {
                    return Err(PlaybackError::UnsupportedSeeking(
                        "prepared byte stream cannot rewind".into(),
                    ));
                }
                let mut decoder = AudioDecoder::open_with_options(
                    source,
                    DecoderOptions {
                        cancellation: cancellation.clone(),
                        ..DecoderOptions::default()
                    },
                )?;
                decoder.seek(position)?;
                stage.cancel_and_join();
                self.input = ActiveInput::Direct {
                    decoder: Box::new(decoder),
                    pcm: VecDeque::new(),
                    ended: false,
                };
            }
        };
        self.consumed = samples_from_duration(position);
        Ok(())
    }
}

impl Drop for ActiveSource {
    fn drop(&mut self) {
        if let ActiveInput::Prepared(stage) = &self.input {
            stage.cancel_and_join();
        }
    }
}

struct Transition {
    next: Arc<PreparedStage>,
    info: AudioStreamInfo,
    identity: StageIdentity,
    fading: PcmCrossfade,
    previous_generation: u64,
    incoming_generation: u64,
    incoming_consumed: u64,
    outgoing_info: AudioStreamInfo,
    transferred: bool,
}

impl Drop for Transition {
    fn drop(&mut self) {
        if !self.transferred {
            self.next.cancel_and_join();
        }
    }
}

struct StageCleanup<'a>(&'a Mutex<Option<Arc<PreparedStage>>>);
impl Drop for StageCleanup<'_> {
    fn drop(&mut self) {
        let stage = lock(self.0).take();
        if let Some(stage) = stage {
            stage.cancel_and_join();
        }
    }
}

/// A dedicated PCM-only worker is selected only when crossfade was configured
/// at session creation. It owns exactly one long-lived MediaOutput instance.
#[allow(clippy::too_many_arguments)]
pub(super) fn worker_entry(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
    stage_slot: Arc<Mutex<Option<Arc<PreparedStage>>>>,
    config: CrossfadeConfig,
) {
    let panic_ready = ready.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        worker_loop(
            Arc::clone(&shared),
            generation,
            source,
            Arc::clone(&output),
            cancellation.clone(),
            ready,
            &stage_slot,
            config,
        );
    }));
    if result.is_err() {
        let error = PlaybackError::WorkerPanicked;
        fail_session(&shared, generation, error.clone(), &cancellation, &output);
        let _ = panic_ready.try_send(Err(error));
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
    stage_slot: &Mutex<Option<Arc<PreparedStage>>>,
    config: CrossfadeConfig,
) {
    let _stage_cleanup = StageCleanup(stage_slot);
    let opened = AudioDecoder::open_with_options(
        source,
        DecoderOptions {
            cancellation: cancellation.clone(),
            ..DecoderOptions::default()
        },
    );
    let decoder = match opened {
        Ok(decoder) => decoder,
        Err(error) => {
            let error: PlaybackError = error.into();
            fail_session(&shared, generation, error.clone(), &cancellation, &output);
            let _ = ready.send(Err(error));
            return;
        }
    };
    let info = decoder.info().clone();
    let mut outgoing_info = info.clone();
    let mut current = ActiveSource::decoder(decoder);
    let graph = {
        let mut data = lock(&shared.data);
        data.processor_initialized = true;
        data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
        data.passthrough_available = false;
        data.filter_graph.clone()
    };
    let mut processor = match PcmProcessor::new(graph.as_deref(), 0) {
        Ok(processor) => processor,
        Err(error) => {
            let error = PlaybackError::FilterGraph(Arc::new(error));
            fail_session(&shared, generation, error.clone(), &cancellation, &output);
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(info)).is_err() || !wait_for_start(&shared, generation) {
        cancellation.cancel();
        return;
    }
    let metrics = shared.observability.metrics();
    metrics.playback_started();
    let _guard = ActivePlaybackGuard { metrics };
    let fade_samples =
        CrossfadeWindow::from_millis(config.duration_ms).expect("validated crossfade settings");
    let mut transition: Option<Transition> = None;
    let mut preparation_requested = false;
    let mut fade_decided = false;

    loop {
        if cancellation.is_cancelled() || is_stopped(&shared, generation) {
            return;
        }
        if let Err(error) = handle_commands(
            &shared,
            generation,
            &output,
            &cancellation,
            &mut current,
            &mut processor,
            &mut transition,
            stage_slot,
            &mut preparation_requested,
        ) {
            if is_stopped(&shared, generation) {
                return;
            }
            fail_session(&shared, generation, error, &cancellation, &output);
            return;
        }
        if is_stopped(&shared, generation) {
            return;
        }

        if !preparation_requested {
            fade_decided = false;
        }
        let source_generation = lock(&shared.data).source_generation;
        if let Some(duration) = current.duration_samples {
            let remaining = duration.saturating_sub(current.consumed);
            let trigger = fade_samples.samples() as u64 + u64::from(config.prepare_before_ms) * 48;
            if !preparation_requested && remaining <= trigger {
                preparation_requested = true;
                shared.publish(CrossfadeEvent::PrepareRequested {
                    generation: source_generation,
                    remaining_ms: remaining / 48,
                });
            }

            // Only a sufficiently buffered, generation-matched source may be
            // promoted. An ignored request takes the original EOF path.
            if !fade_decided && transition.is_none() && remaining <= fade_samples.samples() as u64 {
                fade_decided = true;
                let ready_stage = {
                    let mut staged = lock(stage_slot);
                    if staged.as_ref().is_some_and(|stage| {
                        stage.identity.expected_generation == source_generation
                            && stage.snapshot().state == StageState::Ready
                            && stage.snapshot().info.as_ref().is_some_and(|info| {
                                info.duration.is_none_or(|duration| {
                                    samples_from_duration(duration) >= fade_samples.samples() as u64
                                })
                            })
                    }) {
                        staged.take()
                    } else {
                        None
                    }
                };
                if let Some(next) = ready_stage {
                    let snapshot = next.snapshot();
                    if let Some(info) = snapshot.info {
                        let incoming_generation = {
                            let mut data = lock(&shared.data);
                            data.source_serial = data.source_serial.saturating_add(1);
                            data.source_serial
                        };
                        shared.publish(CrossfadeEvent::TransitionStarted {
                            outgoing_generation: source_generation,
                            incoming_generation,
                            identity: next.identity.clone(),
                            info: info.clone(),
                        });
                        {
                            let mut data = lock(&shared.data);
                            data.source_generation = incoming_generation;
                            data.info = Some(info.clone());
                            data.duration = info.duration;
                        }
                        transition = Some(Transition {
                            next: Arc::clone(&next),
                            info,
                            identity: next.identity.clone(),
                            fading: PcmCrossfade::new(fade_samples),
                            previous_generation: source_generation,
                            incoming_generation,
                            incoming_consumed: 0,
                            outgoing_info: outgoing_info.clone(),
                            transferred: false,
                        });
                    }
                }
            }
        }

        // Split the outgoing frame at the exact sample where the fade starts.
        let until_fade = current
            .duration_samples
            .map(|duration| {
                duration
                    .saturating_sub(fade_samples.samples() as u64)
                    .saturating_sub(current.consumed) as usize
            })
            .filter(|remaining| *remaining > 0)
            .unwrap_or(FRAME_SAMPLES);
        let wanted = transition
            .as_ref()
            .map(|t| FRAME_SAMPLES.min(t.fading.remaining_samples()))
            .unwrap_or(FRAME_SAMPLES.min(until_fade));
        let mut pcm = match current.read(wanted, &cancellation) {
            Ok(Some(data)) => data,
            Ok(None) if transition.is_none() => break,
            Ok(None) => vec![0.0; wanted * 2],
            Err(_) if is_stopped(&shared, generation) => return,
            Err(error) => {
                fail_session(&shared, generation, error, &cancellation, &output);
                return;
            }
        };
        let mut source_position = current.consumed;
        let mut transition_failed = false;
        if let Some(active) = transition.as_mut() {
            let count = pcm.len() / 2;
            let incoming = active.next.buffer.try_pop_exact(count);
            if let Some(incoming) = incoming.filter(|frame| frame.len() == pcm.len()) {
                if let Err(error) = active.fading.mix(&mut pcm, &incoming) {
                    tracing::error!(?error, "crossfade mixer rejected an overlapping frame");
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(OutputError::new("crossfade mixer rejected a frame")),
                        &cancellation,
                        &output,
                    );
                    return;
                }
                active.incoming_consumed += count as u64;
                source_position = active.incoming_consumed;
            } else {
                transition_failed = true;
            }
        }
        if transition_failed {
            if is_stopped(&shared, generation) {
                return;
            }
            let failed = transition.take().expect("active transition exists");
            failed.next.cancel_and_join();
            {
                let mut data = lock(&shared.data);
                data.source_generation = failed.previous_generation;
                data.info = Some(failed.outgoing_info.clone());
                data.duration = failed.outgoing_info.duration;
            }
            shared.publish(CrossfadeEvent::TransitionCancelled {
                outgoing_generation: failed.previous_generation,
                incoming_generation: failed.incoming_generation,
                identity: failed.identity.clone(),
            });
            preparation_requested = true;
        }
        let count = pcm.len() / 2;
        let gain = lock(&shared.data).volume;
        let frame = PcmFrame {
            samples: pcm,
            samples_per_channel: count,
            start_sample: processor.next_sample(),
        };
        if let Err(error) = processor.write_frame(
            frame,
            Some(source_position.saturating_sub(count as u64)),
            true,
            gain,
            |frame, source| {
                deliver_processed_frame(&shared, generation, &output, &cancellation, frame, source)
            },
        ) {
            let error = match error {
                ProcessError::Output(error) => PlaybackError::Output(error),
                ProcessError::Filter(error) => PlaybackError::FilterGraph(Arc::new(error)),
            };
            if is_stopped(&shared, generation) {
                return;
            }
            fail_session(&shared, generation, error, &cancellation, &output);
            return;
        }

        if transition.as_ref().is_some_and(|t| t.fading.complete()) {
            let mut finished = transition.take().expect("checked transition");
            finished.transferred = true;
            let outgoing_generation = finished.previous_generation;
            let incoming_generation = finished.incoming_generation;
            let identity = finished.identity.clone();
            current = ActiveSource::prepared(
                Arc::clone(&finished.next),
                finished.incoming_consumed,
                &finished.info,
            );
            outgoing_info = finished.info.clone();
            preparation_requested = false;
            shared.publish(CrossfadeEvent::TransitionCompleted {
                outgoing_generation,
                incoming_generation,
                identity,
            });
        }
    }

    // Release staged and active decoders before exposing terminal completion.
    // A completion waiter may immediately inspect the stage or reuse the session.
    drop(_stage_cleanup);
    drop(current);

    // Finish is called once, after the final (non-promoted) source reaches EOF.
    let gain = lock(&shared.data).volume;
    let finish = processor
        .finish(gain, |frame, source| {
            deliver_processed_frame(&shared, generation, &output, &cancellation, frame, source)
        })
        .map_err(|error| match error {
            ProcessError::Output(error) => PlaybackError::Output(error),
            ProcessError::Filter(error) => PlaybackError::FilterGraph(Arc::new(error)),
        })
        .and_then(|()| output.finish(&cancellation).map_err(PlaybackError::from));

    match finish {
        Ok(()) => {
            let played = safe_played_position(&output);
            let mut data = lock(&shared.data);
            if data.generation == generation && !data.stopping {
                data.played_position = played.or(data.played_position);
                data.state = PlaybackState::Completed;
                shared.changed.notify_all();
            }
        }
        Err(error) => {
            if !is_stopped(&shared, generation) {
                fail_session(&shared, generation, error, &cancellation, &output);
            }
        }
    }
}

/// Respond to the existing pause/resume/seek/filter command protocol. This
/// logic is deliberately separate from the legacy worker.
#[allow(clippy::too_many_arguments)]
fn handle_commands(
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
