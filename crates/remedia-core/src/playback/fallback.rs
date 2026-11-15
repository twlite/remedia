//! Fallback seeking: replace the active decoder with a second one opened at the seek target.
//!
//! A decoder whose input cannot reposition itself (an HTTP server that ignores `Range`, or a
//! demuxer without seek support) would otherwise answer a seek with an error. When fallback
//! seeking is enabled, the worker instead keeps playing the current source while a short-lived
//! thread opens the same track again, decodes forward and discards audio up to the target. The
//! thread follows the *effective* playhead, so the audio it parks on is where the listener would
//! be by now rather than the position that was asked for. Once it is ready, the worker swaps the
//! decoders at one exact sample, blending a few milliseconds of the audio being cut with the audio
//! being joined so the step cannot click.
//!
//! The replacement is a parked [`AudioDecoder`] rather than a staged PCM buffer. Staging exists to
//! absorb a stalled remote read ahead of an overlap; here the next audio is needed at a position
//! that moves with playback, so the thread keeps the decoder positioned on it and the worker takes
//! the decoder over, as it does for a promoted Opus file. That keeps the memory cost at one decoder
//! and a frame of PCM.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// A replacement that has not parked on its target after this long is abandoned. Preparation
/// only catches up when it decodes faster than playback advances, so a source slower than real
/// time can never succeed.
pub(super) const PREPARATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Audio decoded and discarded before the target so the codec has settled when it is reached.
pub(super) const SETTLE_SAMPLES: u64 = 48_000;

/// Length of the blend between the old and the new decoder, in samples per channel (5 ms).
const SWITCH_RAMP_SAMPLES: usize = 240;

/// How a track is obtained again: the source it was started from, or a caller-supplied
/// [`SourceReopener`] for inputs Remedia cannot reopen by itself.
#[derive(Clone)]
pub(super) struct Reopen {
    /// `None` when the source cannot be opened twice. Never retains a clone of a byte stream:
    /// that would keep its consumer open after the decoder is gone.
    source: Option<InputSource>,
    custom: Option<Arc<dyn SourceReopener>>,
}

impl Reopen {
    pub(super) fn new(source: &InputSource, custom: Option<Arc<dyn SourceReopener>>) -> Self {
        Self {
            source: source.reopen_in_place(),
            custom,
        }
    }

    /// Whether the track can be opened a second time at all.
    pub(super) fn available(&self) -> bool {
        self.source.is_some() || self.custom.is_some()
    }

    fn open(&self, cancellation: &CancellationToken) -> Result<InputSource, PlaybackError> {
        if let Some(custom) = &self.custom {
            return custom.reopen(cancellation).map_err(|error| {
                if cancellation.is_cancelled() {
                    PlaybackError::Cancelled
                } else {
                    PlaybackError::UnsupportedSeeking(format!(
                        "the source could not be opened again: {error}"
                    ))
                }
            });
        }

        self.source.clone().ok_or_else(|| {
            PlaybackError::UnsupportedSeeking(
                "this source cannot be opened a second time to seek".to_owned(),
            )
        })
    }
}

/// Why `reopened` cannot be the track that is playing, if it cannot.
///
/// A reopener or an origin can answer with different media the second time: a plugin that picks
/// a track at random, a URL whose content was replaced. Swapping that in would put the listener
/// somewhere in a different recording, so the stream's shape and length must agree. Duration is
/// allowed a second or one percent of slack, since container metadata differs between opens of
/// an unchanged resource only by rounding. A duration that only one open can determine proves
/// nothing, as when a one-shot stream is reopened as a complete file.
fn track_difference(playing: &AudioStreamInfo, reopened: &AudioStreamInfo) -> Option<&'static str> {
    if playing.codec != reopened.codec {
        return Some("different codec");
    }
    if playing.sample_rate != reopened.sample_rate {
        return Some("different sample rate");
    }
    if playing.channels != reopened.channels {
        return Some("different channel count");
    }

    match (playing.duration, reopened.duration) {
        (Some(left), Some(right)) => {
            let slack = (left / 100).max(Duration::from_secs(1));
            (left.abs_diff(right) > slack).then_some("different duration")
        }
        _ => None,
    }
}

/// The source-timeline start of a decoded frame, in samples.
pub(super) fn frame_start(frame: &DecodedPcmFrame) -> u64 {
    frame
        .source_start_sample
        .unwrap_or(frame.frame.start_sample)
}

/// Drop the first `skip` samples per channel of `frame`, keeping its timeline consistent.
pub(super) fn trim_front(frame: &mut DecodedPcmFrame, skip: usize) {
    let skip = skip.min(frame.frame.samples_per_channel);
    if skip == 0 {
        return;
    }

    let start = frame_start(frame).saturating_add(skip as u64);
    frame.frame.samples.drain(..skip * 2);
    frame.frame.samples_per_channel -= skip;
    frame.frame.start_sample = start;
    frame.source_start_sample = Some(start);
}

/// Blend the head of `incoming` with the audio `outgoing` that is being cut at the same moment.
///
/// The two are different places in the track (that is the seek), so the blend is an equal-power
/// fade over [`SWITCH_RAMP_SAMPLES`]: short enough to be inaudible as a transition, long enough
/// to stop the abrupt step from clicking. With no outgoing audio (the old source had ended) the
/// incoming head fades in from silence.
pub(super) fn blend_head(incoming: &mut [f32], outgoing: Option<&[f32]>) {
    let available = incoming.len() / 2;
    let ramp = match outgoing {
        Some(outgoing) => SWITCH_RAMP_SAMPLES.min(available).min(outgoing.len() / 2),
        None => SWITCH_RAMP_SAMPLES.min(available),
    };

    for index in 0..ramp {
        let angle = (index as f32 + 0.5) / ramp as f32 * std::f32::consts::FRAC_PI_2;
        let (fade_in, fade_out) = angle.sin_cos();
        for channel in 0..2 {
            let position = index * 2 + channel;
            let old = outgoing.map_or(0.0, |outgoing| outgoing[position]);
            incoming[position] = old * fade_out + incoming[position] * fade_in;
        }
    }
}

/// A decoder opened on the same track and parked on decoded audio at the effective playhead.
pub(super) struct Replacement {
    pub(super) decoder: Box<AudioDecoder>,
    head: Option<DecodedPcmFrame>,
    pub(super) info: AudioStreamInfo,
    /// The decoder is bound to the preparation's cancellation token for good. This keeps that
    /// token linked to the playback's, so stopping the playback still interrupts the decoder
    /// after it has taken over.
    _link: Arc<dyn crate::source::CancellationWake>,
}

impl Replacement {
    /// The source sample the parked audio starts at, or `None` if the source already ended.
    pub(super) fn start(&self) -> Option<u64> {
        self.head.as_ref().map(frame_start)
    }

    /// Discard parked audio before `position`, decoding further when the parked frame is not
    /// enough. Afterwards [`Self::start`] is at least `position` (or `None` at end of input).
    pub(super) fn align(&mut self, position: u64) -> Result<(), PlaybackError> {
        loop {
            let Some(head) = self.head.as_mut() else {
                return Ok(());
            };
            let start = frame_start(head);
            if start >= position {
                return Ok(());
            }

            let end = start.saturating_add(head.frame.samples_per_channel as u64);
            if end <= position {
                self.head = self.decoder.next_timed_frame()?;
                continue;
            }

            trim_front(head, (position - start) as usize);

            return Ok(());
        }
    }

    /// Read up to `samples` samples per channel from the parked position onwards, decoding
    /// further as needed. The result is shorter only when the source ends.
    pub(super) fn read_block(&mut self, samples: usize) -> Result<Vec<f32>, PlaybackError> {
        let mut block = Vec::with_capacity(samples * 2);
        while block.len() < samples * 2 {
            let Some(mut head) = self.head.take() else {
                break;
            };
            let wanted = samples - block.len() / 2;
            if head.frame.samples_per_channel <= wanted {
                block.extend_from_slice(&head.frame.samples);
                self.head = self.decoder.next_timed_frame()?;
            } else {
                block.extend_from_slice(&head.frame.samples[..wanted * 2]);
                trim_front(&mut head, wanted);
                self.head = Some(head);
            }
        }

        Ok(block)
    }

    /// Hand over the parked frame.
    pub(super) fn take_head(&mut self) -> Option<DecodedPcmFrame> {
        self.head.take()
    }
}

/// State shared with the preparation thread.
struct PreparationShared {
    /// A replacement was handed to the worker, which now owns the decoder bound to
    /// `cancellation`; cancelling it would stop the playback that is using it.
    handed_over: std::sync::atomic::AtomicBool,
    /// The source sample the replacement should be parked on: the target plus however far the
    /// active source has advanced since the seek was requested.
    required: AtomicU64,
    cancellation: CancellationToken,
    outcome: Mutex<Option<Result<Replacement, PlaybackError>>>,
}

/// Forwards cancellation of the playback to the preparation thread.
struct PreparationLink(CancellationToken);

impl crate::source::CancellationWake for PreparationLink {
    fn wake_for_cancellation(&self) {
        self.0.cancel();
    }
}

/// A seek that is waiting for its replacement source. At most one exists per session.
///
/// Dropping it cancels and joins the preparation thread, so every path out of the worker (stop,
/// failure, a newer seek, a skipped track) releases the second decoder and its connection.
pub(super) struct FallbackSeek {
    request: Option<SeekRequest>,
    target: Duration,
    target_sample: u64,
    origin_sample: u64,
    shared: Arc<PreparationShared>,
    session: Arc<SessionShared>,
    thread: Option<JoinHandle<()>>,
    started: Instant,
    _link: Arc<dyn crate::source::CancellationWake>,
}

impl FallbackSeek {
    /// Begin opening a replacement. `origin_sample` is where the active source is on the source
    /// timeline right now. On failure the request is returned so the caller can answer it.
    pub(super) fn start(
        session: &Arc<SessionShared>,
        reopen: &Reopen,
        request: SeekRequest,
        origin_sample: u64,
        expected: Option<AudioStreamInfo>,
        playback_cancellation: &CancellationToken,
    ) -> Result<Self, (SeekRequest, PlaybackError)> {
        let target = request.position;
        let target_sample = samples_from_duration(target);
        let shared = Arc::new(PreparationShared {
            handed_over: std::sync::atomic::AtomicBool::new(false),
            required: AtomicU64::new(target_sample),
            cancellation: CancellationToken::new(),
            outcome: Mutex::new(None),
        });
        let link: Arc<dyn crate::source::CancellationWake> =
            Arc::new(PreparationLink(shared.cancellation.clone()));
        playback_cancellation.register_waker(Arc::clone(&link));

        let thread_shared = Arc::clone(&shared);
        let thread_session = Arc::clone(session);
        let thread_reopen = reopen.clone();
        let thread_link = Arc::clone(&link);
        let observability = session.observability.clone();
        let spawned = thread::Builder::new()
            .name("remedia-fallback-seek".into())
            .spawn(move || {
                with_dispatch(&observability, || {
                    let result = prepare(
                        &thread_reopen,
                        &thread_shared,
                        target,
                        expected.as_ref(),
                        thread_link,
                    );
                    *lock(&thread_shared.outcome) = Some(result);
                    let mut data = lock(&thread_session.data);
                    data.fallback_signal = true;
                    thread_session.changed.notify_all();
                });
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(error) => {
                return Err((
                    request,
                    PlaybackError::WorkerInitialization(Arc::new(error)),
                ));
            }
        };

        Ok(Self {
            request: Some(request),
            target,
            target_sample,
            origin_sample,
            shared,
            session: Arc::clone(session),
            thread: Some(thread),
            started: Instant::now(),
            _link: link,
        })
    }

    pub(super) fn target(&self) -> Duration {
        self.target
    }

    pub(super) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Tell the preparation thread where the active source is now, so it parks on the audio the
    /// listener would reach at this moment. `source_sample` advances with the source timeline,
    /// so a paused session does not move it and a tempo filter moves it faster than real time.
    pub(super) fn set_playhead(&self, source_sample: u64) {
        let elapsed = source_sample.saturating_sub(self.origin_sample);
        self.shared.required.store(
            self.target_sample.saturating_add(elapsed),
            Ordering::Release,
        );
    }

    /// The source sample the replacement must continue from when the active source is at
    /// `playhead`: the target, plus however far the source has advanced since the seek.
    pub(super) fn position_for(&self, playhead: u64) -> u64 {
        self.target_sample
            .saturating_add(playhead.saturating_sub(self.origin_sample))
    }

    /// The replacement finished preparing, successfully or not.
    pub(super) fn is_finished(&self) -> bool {
        lock(&self.shared.outcome).is_some()
    }

    /// Whether the replacement is parked and ready to take over.
    pub(super) fn is_ready(&self) -> bool {
        lock(&self.shared.outcome)
            .as_ref()
            .is_some_and(Result::is_ok)
    }

    /// Whether the preparation ran out of time without finishing.
    pub(super) fn timed_out(&self) -> bool {
        self.started.elapsed() > PREPARATION_TIMEOUT && !self.is_finished()
    }

    /// Take the finished outcome. `None` while the thread is still working.
    pub(super) fn take_outcome(&self) -> Option<Result<Replacement, PlaybackError>> {
        let outcome = lock(&self.shared.outcome).take();
        if matches!(outcome, Some(Ok(_))) {
            self.shared.handed_over.store(true, Ordering::Release);
        }

        outcome
    }

    /// Claim the seek for application. `false` if it was cancelled first, in which case the
    /// swap must not happen.
    pub(super) fn commit(&self) -> bool {
        self.request
            .as_ref()
            .is_none_or(|request| request.ticket.commit())
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.request
            .as_ref()
            .is_some_and(|request| request.ticket.is_cancelled())
    }

    /// The result to report once the replacement carries on from `applied_sample`.
    pub(super) fn outcome(&self, applied_sample: u64) -> SeekOutcome {
        SeekOutcome {
            requested: self.target,
            applied: duration_from_samples(applied_sample),
            replaced_source: true,
        }
    }

    /// Answer the waiting `seek` call and release the preparation.
    pub(super) fn finish(mut self, result: Result<SeekOutcome, PlaybackError>) {
        if let Some(request) = self.request.take() {
            let _ = request.response.send(result);
        }
    }

    /// A newer request made this seek obsolete; playback continues on the active source.
    pub(super) fn supersede(self) {
        self.finish(Err(PlaybackError::SeekSuperseded));
    }

    /// The caller cancelled this seek before it took effect.
    pub(super) fn cancelled(self) {
        self.finish(Err(PlaybackError::SeekCancelled));
    }
}

impl Drop for FallbackSeek {
    fn drop(&mut self) {
        if !self.shared.handed_over.load(Ordering::Acquire) {
            self.shared.cancellation.cancel();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(request) = self.request.take() {
            let error = lock(&self.session.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::Cancelled);
            let _ = request.response.send(Err(error));
        }
    }
}

/// Open the track again and decode forward to the live target.
fn prepare(
    reopen: &Reopen,
    shared: &PreparationShared,
    target: Duration,
    expected: Option<&AudioStreamInfo>,
    link: Arc<dyn crate::source::CancellationWake>,
) -> Result<Replacement, PlaybackError> {
    let cancellation = &shared.cancellation;
    let source = reopen.open(cancellation)?;
    let mut decoder = AudioDecoder::open_for_playback(
        source,
        DecoderOptions {
            cancellation: cancellation.clone(),
            ..DecoderOptions::default()
        },
    )?;
    let info = decoder.info().clone();
    if let Some(difference) = expected.and_then(|expected| track_difference(expected, &info)) {
        return Err(PlaybackError::UnsupportedSeeking(format!(
            "the reopened source is not the track that is playing ({difference})"
        )));
    }

    // Packets that end well before the target need no decoding, only reading.
    decoder.skip_packets_before(shared.required.load(Ordering::Acquire), SETTLE_SAMPLES)?;

    loop {
        if cancellation.is_cancelled() {
            return Err(PlaybackError::Cancelled);
        }

        let Some(mut frame) = decoder.next_timed_frame()? else {
            return Err(PlaybackError::SeekOutOfRange {
                target,
                duration: info.duration.unwrap_or(Duration::ZERO),
            });
        };

        // The playhead keeps moving while this decodes, so read it for every frame.
        let required = shared.required.load(Ordering::Acquire);
        let start = frame_start(&frame);
        let end = start.saturating_add(frame.frame.samples_per_channel as u64);
        if end <= required {
            continue;
        }

        trim_front(&mut frame, required.saturating_sub(start) as usize);

        return Ok(Replacement {
            decoder: Box::new(decoder),
            head: Some(frame),
            info,
            _link: link,
        });
    }
}

/// Publish and log that a fallback seek was applied.
pub(super) fn record_applied(
    playback_id: u64,
    target: Duration,
    applied_sample: u64,
    prepared_in: Duration,
) {
    let applied = duration_from_samples(applied_sample);
    tracing::info!(
        name: "core.playback.fallback_seek_applied",
        target: "remedia_core::playback",
        playback_id,
        target_ms = target.as_millis() as u64,
        applied_ms = applied.as_millis() as u64,
        compensated_ms = applied.saturating_sub(target).as_millis() as u64,
        prepare_ms = prepared_in.as_millis() as u64,
        "fallback seek replaced the active source"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_has_no_step_at_either_end() {
        let mut incoming = vec![1.0; 960];
        let outgoing = vec![-1.0; 960];

        blend_head(&mut incoming, Some(&outgoing));

        // The first sample is almost entirely the outgoing audio and the last blended sample
        // almost entirely the incoming, so neither edge steps by more than a ramp increment.
        assert!(incoming[0] < -0.95);
        assert!(incoming[2 * SWITCH_RAMP_SAMPLES - 2] > 0.95);
        assert!(
            incoming[2 * SWITCH_RAMP_SAMPLES..]
                .iter()
                .all(|s| *s == 1.0)
        );
    }

    #[test]
    fn the_replacement_position_follows_the_source_timeline() {
        let target = samples_from_duration(Duration::from_secs(90));
        let shared = Arc::new(PreparationShared {
            handed_over: std::sync::atomic::AtomicBool::new(false),
            required: AtomicU64::new(target),
            cancellation: CancellationToken::new(),
            outcome: Mutex::new(None),
        });
        let seek = FallbackSeek {
            request: None,
            target: Duration::from_secs(90),
            target_sample: target,
            origin_sample: 480_000,
            shared,
            session: Arc::clone(&PlaybackSession::new().shared),
            thread: None,
            started: Instant::now(),
            _link: Arc::new(PreparationLink(CancellationToken::new())),
        };

        // Three seconds of source time later, the replacement belongs at 93 s.
        assert_eq!(seek.position_for(480_000 + 3 * 48_000), target + 3 * 48_000);
        // A source that has not moved (paused) keeps the target.
        assert_eq!(seek.position_for(480_000), target);
    }

    #[test]
    fn blending_fades_in_when_there_is_no_outgoing_audio() {
        let mut incoming = vec![1.0; 960];

        blend_head(&mut incoming, None);

        assert!(incoming[0] < 0.05);
        assert!((incoming[2 * SWITCH_RAMP_SAMPLES - 2] - 1.0).abs() < 0.01);
        assert!(
            incoming[2 * SWITCH_RAMP_SAMPLES..]
                .iter()
                .all(|s| *s == 1.0)
        );
    }

    #[test]
    fn blending_never_reads_past_the_shorter_side() {
        let mut incoming = vec![1.0; 20];

        blend_head(&mut incoming, Some(&[0.0; 6]));

        assert_eq!(incoming.len(), 20);
        assert!(incoming[6..].iter().all(|sample| *sample == 1.0));
    }

    #[test]
    fn a_file_is_reopened_but_a_byte_stream_is_not() {
        let file = InputSource::file("track.ogg");
        assert!(Reopen::new(&file, None).available());

        let (stream, _writer) = InputSource::byte_stream(1024).unwrap();
        assert!(!Reopen::new(&stream, None).available());
        assert!(
            !Reopen::new(
                &InputSource::sequential_http_url("https://example.test/a"),
                None
            )
            .available()
        );
    }
}
