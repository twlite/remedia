//! Decoder and processor initialization for single-source playback.

use super::*;

pub(super) struct PreparedWorker<'a> {
    pub(super) output_opus: Option<&'a dyn crate::output::EncodedOpusOutput>,
    pub(super) decoder: AudioDecoder,
    pub(super) processor: PcmProcessor,
    pub(super) can_passthrough: bool,
    pub(super) passthrough_mode: bool,
    pub(super) playback_id: u64,
    pub(super) _active_playback: ActivePlaybackGuard,
}

pub(super) fn prepare<'a>(
    shared: &Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: &'a Arc<dyn MediaOutput>,
    cancellation: &CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
) -> Option<PreparedWorker<'a>> {
    let source_kind = source.kind();
    let decoder_started = Instant::now();
    let open_span = tracing::info_span!(
        target: "remedia_core::decoder",
        "core.media.open",
        source_kind,
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    let decoder_result = open_span.in_scope(|| {
        AudioDecoder::open_for_playback(
            source,
            DecoderOptions {
                cancellation: cancellation.clone(),
                ..DecoderOptions::default()
            },
        )
    });
    let metrics = shared.observability.metrics();
    metrics.record_decoder_startup(decoder_started.elapsed().as_secs_f64());
    let mut decoder = match decoder_result {
        Ok(decoder) => {
            open_span.record("otel.status_code", "OK");
            tracing::info!(
                name: "core.media.opened",
                target: "remedia_core::decoder",
                source_kind,
                stream_index = decoder.info().stream_index,
                codec = %decoder.info().codec,
                source_sample_rate = decoder.info().sample_rate,
                source_channels = decoder.info().channels,
                duration_known = decoder.info().duration.is_some(),
                "audio stream selected"
            );
            decoder
        }
        Err(error) => {
            open_span.record("otel.status_code", "ERROR");
            open_span.record("error.type", error.kind());
            tracing::error!(
                name: "core.media.open_failed",
                target: "remedia_core::decoder",
                source_kind,
                error_kind = error.kind(),
                "media input could not be opened"
            );
            let error = if is_stopped(shared, generation) {
                PlaybackError::Cancelled
            } else {
                error.into()
            };
            fail_session(shared, generation, error.clone(), cancellation, output);
            let _ = ready.send(Err(error));
            return None;
        }
    };

    let graph_expression = {
        let mut data = lock(&shared.data);
        data.processor_initialized = true;
        data.filter_graph.clone()
    };
    let processor = match PcmProcessor::new(graph_expression.as_deref(), 0) {
        Ok(processor) => processor,
        Err(error) => {
            let error = PlaybackError::FilterGraph(Arc::new(error));
            fail_session(shared, generation, error.clone(), cancellation, output);
            let _ = ready.send(Err(error));
            return None;
        }
    };

    let info = decoder.info().clone();
    let output_opus = output.encoded_opus_output();
    let passthrough_rejection = decoder.opus_passthrough_rejection();
    let can_passthrough = output_opus.is_some() && passthrough_rejection.is_none();
    let passthrough_mode =
        can_passthrough && graph_expression.is_none() && lock(&shared.data).volume == 1.0;
    {
        let mut data = lock(&shared.data);
        data.processing_mode = Some(if passthrough_mode {
            PlaybackProcessingMode::Passthrough
        } else {
            PlaybackProcessingMode::Transcoding
        });
        data.passthrough_available = can_passthrough;
        shared.changed.notify_all();
    }
    if can_passthrough {
        tracing::info!(
            name: "core.playback.opus_passthrough_eligible",
            target: "remedia_core::playback",
            playback_id = lock(&shared.data).playback_id,
            "selected Opus stream can use the encoded packet path"
        );
    } else if let Some(reason) = passthrough_rejection {
        tracing::debug!(
            name: "core.playback.opus_passthrough_rejected",
            target: "remedia_core::playback",
            playback_id = lock(&shared.data).playback_id,
            reason,
            "selected stream requires PCM transcoding"
        );
    } else if output_opus.is_none() {
        tracing::trace!(
            name: "core.playback.opus_passthrough_rejected",
            target: "remedia_core::playback",
            playback_id = lock(&shared.data).playback_id,
            reason = "output does not accept encoded Opus",
            "selected output uses the PCM path"
        );
    }
    if !passthrough_mode && let Err(error) = decoder.activate_decoder(&[], None) {
        let error: PlaybackError = error.into();
        fail_session(shared, generation, error.clone(), cancellation, output);
        let _ = ready.send(Err(error));
        return None;
    }
    if ready.send(Ok(info)).is_err() {
        cancellation.cancel();
        let _ = safe_cancel(output);
        return None;
    }
    if !wait_for_start(shared, generation) {
        return None;
    }
    metrics.playback_started();
    let _active_playback = ActivePlaybackGuard { metrics };
    let playback_id = lock(&shared.data).playback_id;
    tracing::debug!(
        name: "core.playback.worker_ready",
        target: "remedia_core::playback",
        playback_id,
        "playback worker entered decode loop"
    );

    Some(PreparedWorker {
        output_opus,
        decoder,
        processor,
        can_passthrough,
        passthrough_mode,
        playback_id,
        _active_playback,
    })
}
