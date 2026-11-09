//! Live transitions between encoded Opus passthrough and PCM processing.

use super::*;
use std::ops::ControlFlow;

/// Borrowed state for a live transition between encoded packets and PCM processing.
pub(super) struct ProcessingTransition<'a> {
    pub(super) shared: &'a Arc<SessionShared>,
    pub(super) generation: u64,
    pub(super) output: &'a Arc<dyn MediaOutput>,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) playback_id: u64,
    pub(super) decoder: &'a mut AudioDecoder,
    pub(super) processor: &'a mut PcmProcessor,
    pub(super) can_passthrough: bool,
    pub(super) passthrough_mode: &'a mut bool,
    pub(super) pending_frame: &'a mut Option<DecodedPcmFrame>,
    pub(super) deferred_filter_update: &'a mut Option<FilterUpdate>,
}

impl ProcessingTransition<'_> {
    pub(super) fn apply(self) -> ControlFlow<()> {
        let Self {
            shared,
            generation,
            output,
            cancellation,
            playback_id,
            decoder,
            processor,
            can_passthrough,
            passthrough_mode,
            pending_frame,
            deferred_filter_update,
        } = self;
        if *passthrough_mode {
            let source_boundary = processor
                .source_position()
                .map(|position| position.sample)
                .or_else(|| decoder.source_boundary_sample())
                .unwrap_or(0);
            let history = decoder.packet_history_before(source_boundary);
            let activation = decoder.activate_decoder(&history, Some(source_boundary));
            if let Err(error) = activation {
                let playback_error: PlaybackError = error.into();
                if can_passthrough {
                    decoder.discard_decoder_activation();
                    {
                        let mut data = lock(&shared.data);
                        data.filter_graph = processor.expression().map(str::to_owned);
                        data.volume = 1.0;
                        data.processing_mode = Some(PlaybackProcessingMode::Passthrough);
                        data.last_processing_error = Some(playback_error.clone());
                        shared.changed.notify_all();
                    }
                    tracing::warn!(
                        name: "core.playback.processing_mode_transition_failed",
                        target: "remedia_core::playback",
                        playback_id,
                        source_boundary_sample = source_boundary,
                        fallback_mode = "passthrough",
                        error_kind = "decoder_initialization",
                        "PCM mode could not be initialized; existing Opus passthrough continues"
                    );
                    if let Some(update) = deferred_filter_update.take() {
                        let _ = update.response.send(Err(playback_error));
                    }
                    return ControlFlow::Continue(());
                }
                if let Some(update) = deferred_filter_update.take() {
                    let _ = update.response.send(Err(playback_error.clone()));
                }
                fail_session(shared, generation, playback_error, cancellation, output);
                return ControlFlow::Break(());
            }
            let applied_filter_update = deferred_filter_update.take();
            if let Some(update) = applied_filter_update {
                processor.replace(update.graph);
                *passthrough_mode = false;
                {
                    let mut data = lock(&shared.data);
                    data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
                    shared.changed.notify_all();
                }
                // Do not acknowledge a filter change until the decoder,
                // graph, and public processing-mode snapshot agree.
                let _ = update.response.send(Ok(()));
            } else {
                *passthrough_mode = false;
                {
                    let mut data = lock(&shared.data);
                    data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
                    shared.changed.notify_all();
                }
            }
            tracing::info!(
                name: "core.playback.processing_mode_changed",
                target: "remedia_core::playback",
                playback_id,
                mode = "transcoding",
                source_boundary_sample = source_boundary,
                warmup_packets = history.len(),
                warmup_policy_ms = 120_u16,
                "playback switched from Opus passthrough to PCM processing"
            );
        } else {
            if decoder.has_pending_frames() {
                match decoder.read_next_timed_frame() {
                    Ok(Some(frame)) => {
                        *pending_frame = Some(frame);
                        return ControlFlow::Continue(());
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let playback_error: PlaybackError = error.into();
                        if let Some(update) = deferred_filter_update.take() {
                            let _ = update.response.send(Err(playback_error.clone()));
                        }
                        fail_session(shared, generation, playback_error, cancellation, output);
                        return ControlFlow::Break(());
                    }
                }
            }
            let next_sample = processor.next_sample();
            let Some(encoded_output) = output.encoded_opus_output() else {
                let error = PlaybackError::Output(OutputError::new(
                    "passthrough output capability disappeared during playback",
                ));
                fail_session(shared, generation, error, cancellation, output);
                return ControlFlow::Break(());
            };
            if let Err(error) = encoded_output.prepare_passthrough(next_sample, cancellation) {
                if is_stopped(shared, generation) {
                    return ControlFlow::Break(());
                }
                fail_session(
                    shared,
                    generation,
                    PlaybackError::Output(error),
                    cancellation,
                    output,
                );
                return ControlFlow::Break(());
            }
            if let Err(error) = decoder.deactivate_decoder() {
                fail_session(shared, generation, error.into(), cancellation, output);
                return ControlFlow::Break(());
            }
            processor.replace(None);
            *passthrough_mode = true;
            {
                let mut data = lock(&shared.data);
                data.processing_mode = Some(PlaybackProcessingMode::Passthrough);
                shared.changed.notify_all();
            }
            if let Some(update) = deferred_filter_update.take() {
                let _ = update.response.send(Ok(()));
            }
            tracing::info!(
                name: "core.playback.processing_mode_changed",
                target: "remedia_core::playback",
                playback_id,
                mode = "passthrough",
                output_sample = next_sample,
                buffered_filter_output = "discarded_at_live_transition",
                "playback switched from PCM processing to Opus passthrough"
            );
        }
        ControlFlow::Continue(())
    }
}
