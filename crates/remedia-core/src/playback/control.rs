use super::*;

pub(super) fn wait_for_start(shared: &SessionShared, generation: u64) -> bool {
    let mut data = lock(&shared.data);
    while data.generation == generation && data.state == PlaybackState::Starting {
        data = wait(&shared.changed, data);
    }
    data.generation == generation && data.state == PlaybackState::Playing
}

pub(super) fn encoded_packet_from_demuxed(
    demuxed: &DemuxedAudioPacket,
    source_channels: Option<u16>,
) -> Option<EncodedOpusPacket> {
    let bytes = demuxed.packet.data()?;
    let parsed = opus2::packet::parse(bytes).ok()?;
    if parsed.frames.is_empty() {
        return None;
    }
    let packet_channels = match opus2::packet::get_nb_channels(bytes).ok()? {
        opus2::Channels::Mono => 1,
        opus2::Channels::Stereo => 2,
    };
    if source_channels != Some(packet_channels) {
        return None;
    }
    let samples = demuxed.samples_per_channel?;
    Some(EncodedOpusPacket {
        bytes: bytes.to_vec(),
        samples_per_channel: samples,
        media_samples_per_channel: samples,
        start_sample: 0,
    })
}

pub(super) fn next_worker_action(
    shared: &SessionShared,
    generation: u64,
    pending_frame: &mut Option<DecodedPcmFrame>,
    input_finished: bool,
    can_passthrough: bool,
    passthrough_mode: bool,
) -> WorkerAction {
    let mut data = lock(&shared.data);
    loop {
        if data.generation != generation || data.stopping || data.state == PlaybackState::Stopped {
            return WorkerAction::Exit;
        }
        let wants_passthrough =
            can_passthrough && data.filter_graph.is_none() && data.volume == 1.0;
        if !passthrough_mode
            && wants_passthrough
            && pending_frame.is_some()
            && data.state == PlaybackState::Playing
        {
            return WorkerAction::Deliver(pending_frame.take().expect("pending frame checked"));
        }
        if let Some(update) = data.pending_filter_graph.take() {
            return WorkerAction::ReplaceFilter(update);
        }
        if let Some(request) = data.pending_seek.take() {
            return WorkerAction::Seek(request);
        }
        if data.state == PlaybackState::Pausing {
            return WorkerAction::Pause;
        }
        if matches!(data.state, PlaybackState::Playing | PlaybackState::Paused)
            && passthrough_mode != wants_passthrough
            && !(wants_passthrough && pending_frame.is_some())
        {
            return WorkerAction::ConfigureProcessing;
        }
        match data.state {
            PlaybackState::Playing => {
                if let Some(frame) = pending_frame.take() {
                    return WorkerAction::Deliver(frame);
                }
                if input_finished {
                    return WorkerAction::Finish;
                }
                return WorkerAction::Decode;
            }
            PlaybackState::Paused | PlaybackState::Starting => {
                data = wait(&shared.changed, data);
            }
            PlaybackState::Resuming => return WorkerAction::Resume,
            PlaybackState::Stopping => return WorkerAction::Exit,
            PlaybackState::Idle
            | PlaybackState::Pausing
            | PlaybackState::Stopped
            | PlaybackState::Completed
            | PlaybackState::Failed => return WorkerAction::Exit,
        }
    }
}

pub(super) fn fail_session(
    shared: &SessionShared,
    generation: u64,
    error: PlaybackError,
    cancellation: &CancellationToken,
    output: &Arc<dyn MediaOutput>,
) {
    // An explicit stop may cancel the source while FFmpeg is still probing.
    // Cancellation must not race ahead of stop() and turn the session into a
    // terminal failure before stop() has a chance to mark it Stopped.
    if cancellation.is_cancelled() || is_stopped(shared, generation) {
        return;
    }

    cancellation.cancel();
    let _ = safe_cancel(output);
    let (pending_seek, pending_filter, playback_id, output_sample) = {
        let mut data = lock(&shared.data);
        if data.generation != generation
            || data.stopping
            || matches!(
                data.state,
                PlaybackState::Stopping
                    | PlaybackState::Stopped
                    | PlaybackState::Completed
                    | PlaybackState::Failed
            )
        {
            return;
        }
        data.state = PlaybackState::Failed;
        data.last_error = Some(error.clone());
        let playback_id = data.playback_id;
        let output_sample = data.submitted_sample;
        let pending_seek = data.pending_seek.take();
        let pending_filter = data.pending_filter_graph.take();
        shared.changed.notify_all();
        (pending_seek, pending_filter, playback_id, output_sample)
    };
    shared.observability.metrics().playback_failed();
    let output_cause = match &error {
        PlaybackError::Output(output_error) => output_error.to_string(),
        _ => String::new(),
    };
    tracing::error!(
        name: "core.playback.failed",
        target: "remedia_core::playback",
        playback_id,
        output_sample,
        error_kind = playback_error_kind(&error),
        output_cause,
        "playback worker failed"
    );
    if let Some(request) = pending_seek {
        let _ = request.response.send(Err(error.clone()));
    }
    if let Some(update) = pending_filter {
        let _ = update.response.send(Err(error));
    }
}

pub(super) fn deliver_processed_frame(
    shared: &SessionShared,
    generation: u64,
    output: &Arc<dyn MediaOutput>,
    cancellation: &CancellationToken,
    frame: PcmFrame,
    source_position: Option<SourcePosition>,
) -> Result<(), OutputError> {
    let end_sample = frame
        .start_sample
        .saturating_add(u64::try_from(frame.samples_per_channel).unwrap_or(u64::MAX));
    output.write_frame(frame, cancellation)?;
    let played = safe_played_position(output);
    let mut data = lock(&shared.data);
    if data.generation == generation && !data.stopping {
        data.submitted_sample = end_sample;
        data.submitted_position = duration_from_samples(end_sample);
        if let Some(source_position) = source_position {
            data.media_position = Some(MediaPosition {
                position: duration_from_samples(source_position.sample),
                accuracy: if source_position.estimated {
                    MediaPositionAccuracy::Estimated
                } else {
                    MediaPositionAccuracy::Timestamp
                },
            });
        }
        if played.is_some() {
            data.played_position = played;
        }
    }
    Ok(())
}

pub(super) fn record_encoded_packet_submitted(
    shared: &SessionShared,
    generation: u64,
    output: &Arc<dyn MediaOutput>,
    end_sample: u64,
    source_position: Option<SourcePosition>,
) {
    let played = safe_played_position(output);
    let mut data = lock(&shared.data);
    if data.generation == generation && !data.stopping {
        data.submitted_sample = end_sample;
        data.submitted_position = duration_from_samples(end_sample);
        if let Some(source_position) = source_position {
            data.media_position = Some(MediaPosition {
                position: duration_from_samples(source_position.sample),
                accuracy: if source_position.estimated {
                    MediaPositionAccuracy::Estimated
                } else {
                    MediaPositionAccuracy::Timestamp
                },
            });
        }
        if played.is_some() {
            data.played_position = played;
        }
    }
}

pub(super) struct ActivePlaybackGuard {
    pub(super) metrics: remedia_observability::RemediaMetrics,
}

impl Drop for ActivePlaybackGuard {
    fn drop(&mut self) {
        self.metrics.playback_stopped();
    }
}

pub(super) fn playback_error_kind(error: &PlaybackError) -> &'static str {
    match error {
        PlaybackError::Decoder(error) => error.kind(),
        PlaybackError::Output(_) => "output",
        PlaybackError::InvalidState { .. } => "invalid_state",
        PlaybackError::WorkerInitialization(_) => "worker_initialization",
        PlaybackError::WorkerPanicked => "worker_panicked",
        PlaybackError::Cancelled => "cancelled",
        PlaybackError::SeekPending => "seek_pending",
        PlaybackError::UnsupportedSeeking(_) => "unsupported_seeking",
        PlaybackError::SeekOutOfRange { .. } => "seek_out_of_range",
        PlaybackError::FilterGraph(_) => "filter_graph",
        PlaybackError::InvalidVolume(_) => "invalid_volume",
        PlaybackError::InvalidCrossfadeConfiguration => "invalid_crossfade_configuration",
    }
}

pub(super) fn is_stopped(shared: &SessionShared, generation: u64) -> bool {
    let data = lock(&shared.data);
    data.generation != generation
        || data.stopping
        || matches!(data.state, PlaybackState::Stopping | PlaybackState::Stopped)
}

pub(super) fn is_active(state: PlaybackState) -> bool {
    matches!(
        state,
        PlaybackState::Starting
            | PlaybackState::Playing
            | PlaybackState::Pausing
            | PlaybackState::Resuming
            | PlaybackState::Paused
            | PlaybackState::Stopping
    )
}

pub(super) fn duration_from_samples(samples: u64) -> Duration {
    let rate = u64::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ);
    let seconds = samples / rate;
    let remainder = samples % rate;
    let nanoseconds = (u128::from(remainder) * 1_000_000_000 / u128::from(rate)) as u32;
    Duration::new(seconds, nanoseconds)
}

pub(super) fn samples_from_duration(position: Duration) -> u64 {
    u64::try_from(
        position.as_nanos() * u128::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000,
    )
    .unwrap_or(u64::MAX)
}

pub(super) fn normalize_position(position: Duration) -> Result<Duration, PlaybackError> {
    let sample_position =
        position.as_nanos() * u128::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    let sample_position = u64::try_from(sample_position).map_err(|_| {
        PlaybackError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
    })?;
    Ok(duration_from_samples(sample_position))
}

pub(super) fn safe_cancel(output: &Arc<dyn MediaOutput>) -> Result<(), ()> {
    catch_unwind(AssertUnwindSafe(|| output.cancel())).map_err(|_| ())
}

pub(super) fn safe_played_position(output: &Arc<dyn MediaOutput>) -> Option<Duration> {
    catch_unwind(AssertUnwindSafe(|| output.played_position()))
        .ok()
        .flatten()
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn wait<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
