use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use remedia_observability::{ObservabilityHandle, next_correlation_id};
use thiserror::Error;

use crate::decoder::{
    AudioDecoder, AudioStreamInfo, DecodedPcmFrame, DecoderOptions, DemuxedAudioPacket, PcmFrame,
    SourcePosition,
};
use crate::error::DecoderError;
use crate::filter::{FilterGraphError, PcmFilterGraph, PcmProcessor, ProcessError};
use crate::opus::EncodedOpusPacket;
use crate::output::{MediaOutput, OutputError};
use crate::source::{CancellationToken, InputSource};
use crate::with_dispatch;

/// Lifecycle state for one playback session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    /// No source has been started in this session.
    Idle,
    /// A worker is opening the source; playback has not started yet.
    Starting,
    /// The worker is decoding and delivering audio.
    Playing,
    /// A pause has been requested and is taking effect at a frame boundary.
    Pausing,
    /// The output is being resumed; frames are not delivered yet.
    Resuming,
    /// Decoding is suspended until [`PlaybackSession::resume`] is called.
    Paused,
    /// Active work is being cancelled and its worker is being joined.
    Stopping,
    /// Active work was stopped and its worker has been joined.
    Stopped,
    /// The decoder reached end-of-stream and the output finished successfully.
    Completed,
    /// An unrecoverable decoder, output, or worker failure occurred.
    Failed,
}

/// The active source-processing path selected for an Opus-capable output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackProcessingMode {
    /// The compressed source Opus payload is forwarded without PCM conversion.
    Passthrough,
    /// The source is decoded and processed as PCM; an Opus-capable output then encodes it.
    Transcoding,
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Playing => "playing",
            Self::Pausing => "pausing",
            Self::Resuming => "resuming",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::Failed => "failed",
        })
    }
}

/// Playback errors retain decoder or output causes where available.
#[derive(Clone, Debug, Error)]
pub enum PlaybackError {
    #[error("decoder error: {0}")]
    Decoder(#[source] Arc<DecoderError>),
    #[error(transparent)]
    Output(#[from] OutputError),
    #[error("cannot {operation} while the session is {state}")]
    InvalidState {
        operation: &'static str,
        state: PlaybackState,
    },
    #[error("playback worker could not be started: {0}")]
    WorkerInitialization(#[source] Arc<std::io::Error>),
    #[error("playback worker panicked")]
    WorkerPanicked,
    #[error("playback operation was cancelled by stop")]
    Cancelled,
    #[error("another seek is already waiting to be applied")]
    SeekPending,
    #[error("seeking is not supported by this source: {0}")]
    UnsupportedSeeking(String),
    #[error("seek position {target:?} is beyond media duration {duration:?}")]
    SeekOutOfRange {
        target: Duration,
        duration: Duration,
    },
    #[error("audio filtergraph error: {0}")]
    FilterGraph(#[source] Arc<FilterGraphError>),
    #[error("master volume must be a finite gain between 0.0 and 2.0, received {0}")]
    InvalidVolume(f32),
}

impl From<DecoderError> for PlaybackError {
    fn from(error: DecoderError) -> Self {
        match error {
            DecoderError::UnsupportedSeeking(message) => Self::UnsupportedSeeking(message),
            DecoderError::SeekOutOfRange { target, duration } => {
                Self::SeekOutOfRange { target, duration }
            }
            error => Self::Decoder(Arc::new(error)),
        }
    }
}

/// Positions reported by a session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaybackPosition {
    /// Source/media position associated with the latest output boundary. This
    /// is a processing estimate and does not claim samples have been heard.
    pub media: Option<MediaPosition>,
    /// The end of the latest frame accepted by the output.
    pub submitted: Duration,
    /// The output's consumed/audible position, when it can report one.
    pub played: Option<Duration>,
}

/// Source timeline position attached to submitted filtered output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaPosition {
    /// Position in the original media timeline, before time-changing filters.
    pub position: Duration,
    /// Whether the position came directly from source timestamps or from
    /// continuity inferred across decoded frames / filtered output.
    pub accuracy: MediaPositionAccuracy,
}

/// Confidence of a source-media position estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaPositionAccuracy {
    /// Derived from a valid FFmpeg source timestamp on the associated frame.
    Timestamp,
    /// Inferred because timestamps were missing or a filtergraph can buffer or
    /// change the mapping between source and output samples.
    Estimated,
}

/// A single-source playback session.
///
/// Each active session owns a blocking worker thread, one demux reader, and one
/// output instance. It creates a decoder only when PCM is required. Frames or
/// encoded packets are delivered synchronously, so backpressure stops further
/// input without a session-level audio queue. A new `play` call is rejected
/// while a source is active; call `stop` before starting a replacement.
pub struct PlaybackSession {
    shared: Arc<SessionShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    start_lock: Mutex<()>,
    filter_update_lock: Mutex<()>,
}

/// A completion wait bound to the playback generation active when it was
/// created. Replacing a stopped source cannot make an older waiter follow the
/// new source.
pub struct PlaybackWaiter {
    shared: Arc<SessionShared>,
    generation: u64,
}

struct SessionShared {
    data: Mutex<SessionData>,
    changed: Condvar,
    observability: ObservabilityHandle,
}

struct SessionData {
    state: PlaybackState,
    generation: u64,
    playback_id: u64,
    processing_mode: Option<PlaybackProcessingMode>,
    passthrough_available: bool,
    last_processing_error: Option<PlaybackError>,
    duration: Option<Duration>,
    info: Option<AudioStreamInfo>,
    submitted_position: Duration,
    submitted_sample: u64,
    media_position: Option<MediaPosition>,
    played_position: Option<Duration>,
    last_error: Option<PlaybackError>,
    cancellation: Option<CancellationToken>,
    output: Option<Arc<dyn MediaOutput>>,
    pending_seek: Option<SeekRequest>,
    filter_graph: Option<String>,
    pending_filter_graph: Option<FilterUpdate>,
    processor_initialized: bool,
    volume: f32,
    stopping: bool,
}

struct SeekRequest {
    position: Duration,
    response: SyncSender<Result<(), PlaybackError>>,
}

struct FilterUpdate {
    graph: Option<PcmFilterGraph>,
    response: SyncSender<Result<(), PlaybackError>>,
}

enum WorkerAction {
    ConfigureProcessing,
    Decode,
    Deliver(DecodedPcmFrame),
    Seek(SeekRequest),
    ReplaceFilter(FilterUpdate),
    Pause,
    Resume,
    Finish,
    Exit,
}

impl Default for PlaybackSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackSession {
    /// Create an idle playback session. Sessions do not share playback state.
    pub fn new() -> Self {
        Self::with_observability(ObservabilityHandle::default())
    }

    /// Create an idle session attached to an application-owned observability context.
    ///
    /// The handle controls which local tracing subscriber and metric provider receive events. It
    /// does not install a process-global subscriber.
    pub fn with_observability(observability: ObservabilityHandle) -> Self {
        Self {
            shared: Arc::new(SessionShared {
                data: Mutex::new(SessionData {
                    state: PlaybackState::Idle,
                    generation: 0,
                    playback_id: 0,
                    processing_mode: None,
                    passthrough_available: false,
                    last_processing_error: None,
                    duration: None,
                    info: None,
                    submitted_position: Duration::ZERO,
                    submitted_sample: 0,
                    media_position: None,
                    played_position: None,
                    last_error: None,
                    cancellation: None,
                    output: None,
                    pending_seek: None,
                    filter_graph: None,
                    pending_filter_graph: None,
                    processor_initialized: false,
                    volume: 1.0,
                    stopping: false,
                }),
                changed: Condvar::new(),
                observability,
            }),
            worker: Mutex::new(None),
            start_lock: Mutex::new(()),
            filter_update_lock: Mutex::new(()),
        }
    }

    /// Open a source and start it with an output owned by this playback.
    ///
    /// This call blocks until the FFmpeg input and audio decoder are initialized.
    /// It returns the selected stream metadata only after initialization succeeds.
    /// Calls from another thread may stop a source while it is opening.
    pub fn play<O: MediaOutput>(
        &self,
        source: InputSource,
        output: O,
    ) -> Result<AudioStreamInfo, PlaybackError> {
        self.play_with_cancellation(source, output, CancellationToken::new())
    }

    /// Open a source using a caller-owned cancellation signal.
    ///
    /// If the token is cancelled before the worker starts, playback does not
    /// begin. Once started, cancelling the token interrupts decoder I/O; call
    /// [`stop`](Self::stop) to join the worker and release its output.
    pub fn play_with_cancellation<O: MediaOutput>(
        &self,
        source: InputSource,
        output: O,
        cancellation: CancellationToken,
    ) -> Result<AudioStreamInfo, PlaybackError> {
        let start_guard = lock(&self.start_lock);
        if cancellation.is_cancelled() {
            return Err(PlaybackError::Cancelled);
        }
        {
            let data = lock(&self.shared.data);
            if cancellation.is_cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if data.stopping || is_active(data.state) {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }
        }
        self.reap_previous_worker()?;
        if cancellation.is_cancelled() {
            return Err(PlaybackError::Cancelled);
        }

        let output: Arc<dyn MediaOutput> = Arc::new(output);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let source_kind = source.kind();
        let (generation, playback_id) = {
            let mut data = lock(&self.shared.data);
            if cancellation.is_cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if data.stopping {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }
            if is_active(data.state) {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }

            data.generation = data.generation.wrapping_add(1);
            let generation = data.generation;
            let playback_id = next_correlation_id();
            data.playback_id = playback_id;
            data.processing_mode = None;
            data.passthrough_available = false;
            data.last_processing_error = None;
            data.state = PlaybackState::Starting;
            data.duration = None;
            data.info = None;
            data.submitted_position = Duration::ZERO;
            data.submitted_sample = 0;
            data.media_position = None;
            data.played_position = None;
            data.last_error = None;
            data.cancellation = Some(cancellation.clone());
            data.output = Some(Arc::clone(&output));
            data.pending_seek = None;
            data.processor_initialized = false;

            let shared = Arc::clone(&self.shared);
            let worker_output = Arc::clone(&output);
            let worker_cancellation = cancellation.clone();
            let worker_observability = self.shared.observability.clone();
            let thread_result = thread::Builder::new()
                .name(format!("remedia-playback-{generation}"))
                .spawn(move || {
                    with_dispatch(&worker_observability, || {
                        let span = tracing::info_span!(
                            target: "remedia_core::playback",
                            "core.playback.session",
                            playback_id,
                            generation,
                            otel.status_code = tracing::field::Empty,
                            error.type = tracing::field::Empty,
                        );
                        let _entered = span.enter();
                        let span_shared = Arc::clone(&shared);
                        worker_entry(
                            shared,
                            generation,
                            source,
                            worker_output,
                            worker_cancellation,
                            ready_tx,
                        );
                        let data = lock(&span_shared.data);
                        if data.generation == generation {
                            match data.state {
                                PlaybackState::Completed => {
                                    span.record("otel.status_code", "OK");
                                }
                                PlaybackState::Failed => {
                                    span.record("otel.status_code", "ERROR");
                                    if let Some(error) = &data.last_error {
                                        span.record("error.type", playback_error_kind(error));
                                    }
                                }
                                _ => {}
                            }
                        }
                    });
                });

            match thread_result {
                Ok(worker) => {
                    *lock(&self.worker) = Some(worker);
                    self.shared.changed.notify_all();
                }
                Err(error) => {
                    let playback_error = PlaybackError::WorkerInitialization(Arc::new(error));
                    data.state = PlaybackState::Failed;
                    data.last_error = Some(playback_error.clone());
                    data.cancellation = None;
                    data.output = None;
                    self.shared.changed.notify_all();
                    drop(data);
                    drop(start_guard);
                    self.shared.observability.metrics().playback_failed();
                    with_dispatch(&self.shared.observability, || {
                        tracing::error!(
                            name: "core.playback.worker_initialization_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            error_kind = "worker_initialization",
                            "playback worker could not be started"
                        );
                    });
                    let _ = safe_cancel(&output);
                    return Err(playback_error);
                }
            };
            (generation, playback_id)
        };

        drop(start_guard);
        with_dispatch(&self.shared.observability, || {
            tracing::info!(
                name: "core.playback.starting",
                target: "remedia_core::playback",
                playback_id,
                source_kind,
                "playback source opening"
            );
        });

        match ready_rx.recv() {
            Ok(Ok(info)) => {
                let mut data = lock(&self.shared.data);
                if data.generation != generation || data.state != PlaybackState::Starting {
                    return Err(data.last_error.clone().unwrap_or(PlaybackError::Cancelled));
                }
                data.duration = info.duration;
                data.info = Some(info.clone());
                data.state = PlaybackState::Playing;
                self.shared.changed.notify_all();
                drop(data);
                with_dispatch(&self.shared.observability, || {
                    tracing::info!(
                        name: "core.playback.started",
                        target: "remedia_core::playback",
                        playback_id,
                        codec = %info.codec,
                        duration_known = info.duration.is_some(),
                        "playback started"
                    );
                });
                Ok(info)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
        }
    }

    /// Request a pause and wait until the worker reaches a frame boundary.
    ///
    /// The call can wait for an in-progress decoder read or output write to
    /// return. The worker then waits on a condition variable and does not decode
    /// or queue more audio until resumed.
    pub fn pause(&self) -> Result<(), PlaybackError> {
        let mut data = lock(&self.shared.data);
        match data.state {
            PlaybackState::Paused => return Ok(()),
            PlaybackState::Playing => {
                data.state = PlaybackState::Pausing;
                self.shared.changed.notify_all();
            }
            PlaybackState::Pausing => {}
            state => {
                return Err(PlaybackError::InvalidState {
                    operation: "pause",
                    state,
                });
            }
        }

        while data.state == PlaybackState::Pausing {
            data = wait(&self.shared.changed, data);
        }
        match data.state {
            PlaybackState::Paused => Ok(()),
            PlaybackState::Failed => Err(data
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
            PlaybackState::Stopping => Err(PlaybackError::Cancelled),
            PlaybackState::Stopped => Err(PlaybackError::Cancelled),
            state => Err(PlaybackError::InvalidState {
                operation: "pause",
                state,
            }),
        }
    }

    /// Resume a paused session. Calling this while already playing is harmless.
    pub fn resume(&self) -> Result<(), PlaybackError> {
        let mut data = lock(&self.shared.data);
        match data.state {
            PlaybackState::Playing => return Ok(()),
            PlaybackState::Paused => {
                data.state = PlaybackState::Resuming;
                self.shared.changed.notify_all();
            }
            PlaybackState::Resuming => {}
            state => {
                return Err(PlaybackError::InvalidState {
                    operation: "resume",
                    state,
                });
            }
        }

        while data.state == PlaybackState::Resuming {
            data = wait(&self.shared.changed, data);
        }
        match data.state {
            PlaybackState::Playing => Ok(()),
            PlaybackState::Failed => Err(data
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
            PlaybackState::Stopping | PlaybackState::Stopped => Err(PlaybackError::Cancelled),
            state => Err(PlaybackError::InvalidState {
                operation: "resume",
                state,
            }),
        }
    }

    /// Seek the active source. Seeking is supported while playing or paused.
    /// The call waits for the decoder and output to discard pre-seek data.
    pub fn seek(&self, position: Duration) -> Result<(), PlaybackError> {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let playback_id = {
            let mut data = lock(&self.shared.data);
            if !matches!(data.state, PlaybackState::Playing | PlaybackState::Paused) {
                return Err(PlaybackError::InvalidState {
                    operation: "seek",
                    state: data.state,
                });
            }
            if data.pending_seek.is_some() {
                return Err(PlaybackError::SeekPending);
            }
            if let Some(duration) = data.duration
                && position > duration
            {
                return Err(PlaybackError::SeekOutOfRange {
                    target: position,
                    duration,
                });
            }
            let position = normalize_position(position)?;
            data.pending_seek = Some(SeekRequest {
                position,
                response: response_tx,
            });
            self.shared.changed.notify_all();
            data.playback_id
        };
        with_dispatch(&self.shared.observability, || {
            tracing::info!(
                name: "core.playback.seek_requested",
                target: "remedia_core::playback",
                playback_id,
                position_ms = position.as_millis() as u64,
                "playback seek requested"
            );
        });

        response_rx.recv().unwrap_or_else(|_| {
            Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked))
        })
    }

    /// Validate and configure a trusted FFmpeg audio filtergraph.
    ///
    /// The expression is parsed by native libavfilter and must describe one
    /// audio input and one audio output. `None` clears filtering. Empty or
    /// whitespace-only strings are rejected. When active, replacement waits
    /// for a PCM boundary and drops delayed output still held inside the
    /// previous graph. PCM already accepted by the output is retained. The
    /// source is not reopened. Graph strings are trusted application
    /// configuration; some FFmpeg filters can access files or URLs.
    pub fn set_filter_graph(&self, expression: Option<&str>) -> Result<(), PlaybackError> {
        let _update = lock(&self.filter_update_lock);
        let expression = expression.map(str::to_owned);
        let candidate = with_dispatch(&self.shared.observability, || {
            expression.as_deref().map(PcmFilterGraph::build).transpose()
        });
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                let playback_id = lock(&self.shared.data).playback_id;
                with_dispatch(&self.shared.observability, || {
                    tracing::warn!(
                        name: "core.playback.filter_graph_replacement_failed",
                        target: "remedia_core::playback",
                        playback_id,
                        error_kind = "invalid_graph",
                        "replacement filtergraph could not be built; active graph was retained"
                    );
                });
                return Err(PlaybackError::FilterGraph(Arc::new(error)));
            }
        };
        let mut data = lock(&self.shared.data);
        if data.filter_graph == expression {
            return Ok(());
        }
        data.last_processing_error = None;
        let playback_id = data.playback_id;
        let graph_enabled = expression.is_some();
        data.filter_graph = expression;
        let active = is_active(data.state) && !data.stopping;
        let response = if active && data.processor_initialized {
            let (sender, receiver) = mpsc::sync_channel(1);
            data.pending_filter_graph = Some(FilterUpdate {
                graph: candidate,
                response: sender,
            });
            self.shared.changed.notify_all();
            Some(receiver)
        } else {
            None
        };
        drop(data);
        with_dispatch(&self.shared.observability, || {
            tracing::info!(
                name: "core.playback.filter_graph_replacement_requested",
                target: "remedia_core::playback",
                playback_id,
                graph_enabled,
                active,
                "filtergraph replacement requested"
            );
        });
        if let Some(receiver) = response {
            return receiver.recv().unwrap_or_else(|_| {
                let data = lock(&self.shared.data);
                if data.stopping
                    || matches!(data.state, PlaybackState::Stopping | PlaybackState::Stopped)
                {
                    Err(PlaybackError::Cancelled)
                } else {
                    Err(data
                        .last_error
                        .clone()
                        .unwrap_or(PlaybackError::WorkerPanicked))
                }
            });
        }
        Ok(())
    }

    /// Return the configured graph expression, if any.
    pub fn filter_graph(&self) -> Option<String> {
        lock(&self.shared.data).filter_graph.clone()
    }

    /// Set master gain independently of the raw FFmpeg filtergraph.
    ///
    /// The accepted linear amplitude range is `0.0..=2.0`. Samples are hard
    /// clipped to `[-1.0, 1.0]`; this is not a limiter or loudness normalizer.
    /// Changes take effect at the next processed PCM frame and persist across
    /// tracks in this session. If the change requires entering or leaving Opus
    /// passthrough, this call waits for that worker transition and returns its
    /// decoder/output error. The wait can include a blocking input read or
    /// bounded-output backpressure; `stop` cancels it. While paused, the value
    /// is staged for the resume boundary instead of making this call wait.
    pub fn set_volume(&self, gain: f32) -> Result<(), PlaybackError> {
        if !gain.is_finite() || !(0.0..=2.0).contains(&gain) {
            return Err(PlaybackError::InvalidVolume(gain));
        }
        let (generation, target_mode) = {
            let mut data = lock(&self.shared.data);
            if data.volume == gain {
                return Ok(());
            }
            data.last_processing_error = None;
            data.volume = gain;
            let target_mode = data.passthrough_available.then_some(
                if data.filter_graph.is_none() && gain == 1.0 {
                    PlaybackProcessingMode::Passthrough
                } else {
                    PlaybackProcessingMode::Transcoding
                },
            );
            let target_mode = target_mode.filter(|target| {
                data.processor_initialized
                    && data.state == PlaybackState::Playing
                    && data.processing_mode != Some(*target)
            });
            self.shared.changed.notify_all();
            (data.generation, target_mode)
        };

        if let Some(target_mode) = target_mode {
            let mut data = lock(&self.shared.data);
            loop {
                if data.generation != generation {
                    return Err(PlaybackError::Cancelled);
                }
                if let Some(error) = data.last_processing_error.take() {
                    return Err(error);
                }
                if data.processing_mode == Some(target_mode) {
                    return Ok(());
                }
                if data.stopping
                    || matches!(data.state, PlaybackState::Stopping | PlaybackState::Stopped)
                {
                    return Err(PlaybackError::Cancelled);
                }
                if data.state == PlaybackState::Failed {
                    return Err(data
                        .last_error
                        .clone()
                        .unwrap_or(PlaybackError::WorkerPanicked));
                }
                if !is_active(data.state) {
                    // Natural EOF may win the race. The configured gain is
                    // retained for the next source on this session.
                    return Ok(());
                }
                data = wait(&self.shared.changed, data);
            }
        }
        Ok(())
    }

    /// Current normalized linear master gain.
    pub fn volume(&self) -> f32 {
        lock(&self.shared.data).volume
    }

    /// Validate a graph without creating a playback session.
    pub fn validate_filter_graph(expression: &str) -> Result<(), FilterGraphError> {
        PcmFilterGraph::build(expression).map(|_| ())
    }

    /// Stop active work, cancel pending I/O/output, and join its worker.
    ///
    /// Stopping an idle session changes it to `Stopped`. Repeated stops are
    /// harmless. `Completed` and `Failed` remain terminal so their outcome and
    /// error remain observable.
    pub fn stop(&self) -> Result<(), PlaybackError> {
        let (
            needs_cleanup,
            active,
            playback_id,
            cancellation,
            output,
            pending_seek,
            pending_filter,
        ) = {
            let mut data = lock(&self.shared.data);
            while data.stopping {
                data = wait(&self.shared.changed, data);
            }

            let active = is_active(data.state);
            let needs_cleanup =
                active || matches!(data.state, PlaybackState::Completed | PlaybackState::Failed);
            if data.state == PlaybackState::Idle {
                data.state = PlaybackState::Stopped;
            }
            if needs_cleanup {
                data.stopping = true;
            }
            if active {
                data.state = PlaybackState::Stopping;
            }
            let cancellation = if active {
                data.cancellation.clone()
            } else {
                None
            };
            let output = data.output.take();
            let pending_seek = data.pending_seek.take();
            let pending_filter = data.pending_filter_graph.take();
            let playback_id = data.playback_id;
            self.shared.changed.notify_all();
            (
                needs_cleanup,
                active,
                playback_id,
                cancellation,
                output,
                pending_seek,
                pending_filter,
            )
        };

        if let Some(request) = pending_seek {
            let _ = request.response.send(Err(PlaybackError::Cancelled));
        }
        if let Some(update) = pending_filter {
            let _ = update.response.send(Err(PlaybackError::Cancelled));
        }
        if let Some(cancellation) = &cancellation {
            with_dispatch(&self.shared.observability, || {
                tracing::debug!(
                    name: "core.playback.cancellation_requested",
                    target: "remedia_core::playback",
                    playback_id,
                    "playback cancellation requested"
                );
            });
            cancellation.cancel();
        }
        let output_cancel_panicked = cancellation.is_some()
            && output
                .as_ref()
                .is_some_and(|output| safe_cancel(output).is_err());

        let worker = lock(&self.worker).take();
        let worker_panicked = worker.is_some_and(|worker| worker.join().is_err());
        let played_position = output.as_ref().and_then(safe_played_position);

        let mut data = lock(&self.shared.data);
        if let Some(position) = played_position {
            data.played_position = Some(position);
        }
        data.cancellation = None;
        data.output = None;
        if needs_cleanup {
            data.stopping = false;
        }
        if output_cancel_panicked || worker_panicked {
            let error = PlaybackError::WorkerPanicked;
            data.state = PlaybackState::Failed;
            data.last_error = Some(error.clone());
            self.shared.changed.notify_all();
            return Err(error);
        }
        if active {
            data.state = PlaybackState::Stopped;
        }
        let terminal_state = data.state;
        self.shared.changed.notify_all();
        drop(data);
        if needs_cleanup {
            with_dispatch(&self.shared.observability, || {
                tracing::info!(
                    name: "core.playback.stopped",
                    target: "remedia_core::playback",
                    playback_id,
                    state = %terminal_state,
                    "playback worker stopped and resources released"
                );
            });
        }
        Ok(())
    }

    /// Return the current typed lifecycle state.
    pub fn state(&self) -> PlaybackState {
        lock(&self.shared.data).state
    }

    /// Return the current source processing mode when a playback source has
    /// started. `None` is returned before startup selects a path.
    pub fn processing_mode(&self) -> Option<PlaybackProcessingMode> {
        lock(&self.shared.data).processing_mode
    }

    /// Return submitted position and an optional output-reported played
    /// position. A buffering output may report `None` until it can determine
    /// which samples have actually played.
    pub fn position(&self) -> PlaybackPosition {
        let (media, submitted, cached_played, output) = {
            let data = lock(&self.shared.data);
            (
                data.media_position,
                data.submitted_position,
                data.played_position,
                data.output.clone(),
            )
        };
        let played = output
            .as_ref()
            .and_then(safe_played_position)
            .or(cached_played);
        PlaybackPosition {
            media,
            submitted,
            played,
        }
    }

    /// Return the selected media stream's duration when it is known.
    pub fn duration(&self) -> Option<Duration> {
        lock(&self.shared.data).duration
    }

    /// Return the selected stream metadata after startup succeeds.
    pub fn info(&self) -> Option<AudioStreamInfo> {
        lock(&self.shared.data).info.clone()
    }

    /// Return the most recent background failure, if the session failed.
    pub fn last_error(&self) -> Option<PlaybackError> {
        lock(&self.shared.data).last_error.clone()
    }

    /// Block until the session reaches a terminal state.
    pub fn wait(&self) -> PlaybackState {
        self.completion().wait()
    }

    /// Capture the active playback so a wait scheduled on another thread stays
    /// bound to that source even if it is stopped and replaced before waiting
    /// begins.
    pub fn completion(&self) -> PlaybackWaiter {
        self.completion_for_generation(self.completion_generation())
    }

    /// Return the current source generation for adapters that must capture a wait
    /// before scheduling blocking work on another runtime.
    pub fn completion_generation(&self) -> u64 {
        lock(&self.shared.data).generation
    }

    /// Bind a completion waiter to a generation captured by an adapter.
    pub fn completion_for_generation(&self, generation: u64) -> PlaybackWaiter {
        PlaybackWaiter {
            shared: Arc::clone(&self.shared),
            generation,
        }
    }

    fn reap_previous_worker(&self) -> Result<(), PlaybackError> {
        let previous = lock(&self.worker).take();
        if previous.is_some_and(|worker| worker.join().is_err()) {
            return Err(PlaybackError::WorkerPanicked);
        }
        Ok(())
    }
}

impl PlaybackWaiter {
    /// Block until the captured source reaches a terminal state. If it has
    /// already been replaced, this returns `Stopped`.
    pub fn wait(&self) -> PlaybackState {
        let mut data = lock(&self.shared.data);
        while data.generation == self.generation && is_active(data.state) {
            data = wait(&self.shared.changed, data);
        }
        if data.generation != self.generation {
            return PlaybackState::Stopped;
        }
        data.state
    }
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn worker_entry(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
) {
    let panic_ready = ready.clone();
    let worker_shared = Arc::clone(&shared);
    let worker_output = Arc::clone(&output);
    let worker_cancellation = cancellation.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        worker_loop(shared, generation, source, output, cancellation, ready);
    }));

    if result.is_err() {
        let error = PlaybackError::WorkerPanicked;
        worker_cancellation.cancel();
        fail_session(
            &worker_shared,
            generation,
            error.clone(),
            &worker_cancellation,
            &worker_output,
        );
        let _ = panic_ready.try_send(Err(error));
    }
}

fn worker_loop(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
) {
    let _log_capture = crate::ffmpeg_logging::capture();
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
            let error = if is_stopped(&shared, generation) {
                PlaybackError::Cancelled
            } else {
                error.into()
            };
            fail_session(&shared, generation, error.clone(), &cancellation, &output);
            let _ = ready.send(Err(error));
            return;
        }
    };

    let graph_expression = {
        let mut data = lock(&shared.data);
        data.processor_initialized = true;
        data.filter_graph.clone()
    };
    let mut processor = match PcmProcessor::new(graph_expression.as_deref(), 0) {
        Ok(processor) => processor,
        Err(error) => {
            let error = PlaybackError::FilterGraph(Arc::new(error));
            fail_session(&shared, generation, error.clone(), &cancellation, &output);
            let _ = ready.send(Err(error));
            return;
        }
    };

    let info = decoder.info().clone();
    let output_opus = output.encoded_opus_output();
    let passthrough_rejection = decoder.opus_passthrough_rejection();
    let mut can_passthrough = output_opus.is_some() && passthrough_rejection.is_none();
    let mut passthrough_mode =
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
        fail_session(&shared, generation, error.clone(), &cancellation, &output);
        let _ = ready.send(Err(error));
        return;
    }
    if ready.send(Ok(info)).is_err() {
        cancellation.cancel();
        let _ = safe_cancel(&output);
        return;
    }
    if !wait_for_start(&shared, generation) {
        return;
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

    let mut pending_frame = None;
    let mut deferred_filter_update: Option<FilterUpdate> = None;
    let mut input_finished = false;
    loop {
        match next_worker_action(
            &shared,
            generation,
            &mut pending_frame,
            input_finished,
            can_passthrough,
            passthrough_mode,
        ) {
            WorkerAction::Exit => return,
            WorkerAction::ConfigureProcessing => {
                if passthrough_mode {
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
                            continue;
                        }
                        if let Some(update) = deferred_filter_update.take() {
                            let _ = update.response.send(Err(playback_error.clone()));
                        }
                        fail_session(&shared, generation, playback_error, &cancellation, &output);
                        return;
                    }
                    let applied_filter_update = deferred_filter_update.take();
                    if let Some(update) = applied_filter_update {
                        processor.replace(update.graph);
                        passthrough_mode = false;
                        {
                            let mut data = lock(&shared.data);
                            data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
                            shared.changed.notify_all();
                        }
                        // Do not acknowledge a filter change until the decoder,
                        // graph, and public processing-mode snapshot agree.
                        let _ = update.response.send(Ok(()));
                    } else {
                        passthrough_mode = false;
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
                                pending_frame = Some(frame);
                                continue;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                let playback_error: PlaybackError = error.into();
                                if let Some(update) = deferred_filter_update.take() {
                                    let _ = update.response.send(Err(playback_error.clone()));
                                }
                                fail_session(
                                    &shared,
                                    generation,
                                    playback_error,
                                    &cancellation,
                                    &output,
                                );
                                return;
                            }
                        }
                    }
                    let next_sample = processor.next_sample();
                    let Some(encoded_output) = output.encoded_opus_output() else {
                        let error = PlaybackError::Output(OutputError::new(
                            "passthrough output capability disappeared during playback",
                        ));
                        fail_session(&shared, generation, error, &cancellation, &output);
                        return;
                    };
                    if let Err(error) =
                        encoded_output.prepare_passthrough(next_sample, &cancellation)
                    {
                        if is_stopped(&shared, generation) {
                            return;
                        }
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                    if let Err(error) = decoder.deactivate_decoder() {
                        fail_session(&shared, generation, error.into(), &cancellation, &output);
                        return;
                    }
                    processor.replace(None);
                    passthrough_mode = true;
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
            }
            WorkerAction::Decode => {
                if passthrough_mode {
                    match decoder.read_compressed_packet() {
                        Ok(Some(packet)) => {
                            let should_process = {
                                let data = lock(&shared.data);
                                data.filter_graph.is_some() || data.volume != 1.0
                            };
                            if should_process {
                                decoder.queue_compressed_packet(packet);
                                continue;
                            }
                            let packet_info =
                                encoded_packet_from_demuxed(&packet, decoder.info().channels);
                            let accepted = output_opus
                                .filter(|_| !packet.has_trimming_metadata)
                                .zip(packet_info.as_ref())
                                .is_some_and(|(encoded, packet)| {
                                    encoded.supports_encoded_packet(packet)
                                });
                            if accepted {
                                let Some(packet_info) = packet_info else {
                                    continue;
                                };
                                let start_sample = match processor.record_passthrough_packet(
                                    packet_info.samples_per_channel,
                                    packet.source_start_sample,
                                    packet.source_position_estimated,
                                ) {
                                    Ok(start_sample) => start_sample,
                                    Err(error) => {
                                        fail_session(
                                            &shared,
                                            generation,
                                            PlaybackError::FilterGraph(Arc::new(error)),
                                            &cancellation,
                                            &output,
                                        );
                                        return;
                                    }
                                };
                                let packet_info = EncodedOpusPacket {
                                    start_sample,
                                    ..packet_info
                                };
                                let submitted_end = start_sample.saturating_add(u64::from(
                                    packet_info.media_samples_per_channel,
                                ));
                                let source_end = packet.source_start_sample.map(|start| {
                                    start.saturating_add(u64::from(
                                        packet_info.media_samples_per_channel,
                                    ))
                                });
                                if let Some(encoded) = output_opus
                                    && let Err(error) =
                                        encoded.write_encoded_packet(packet_info, &cancellation)
                                {
                                    if is_stopped(&shared, generation) {
                                        return;
                                    }
                                    fail_session(
                                        &shared,
                                        generation,
                                        PlaybackError::Output(error),
                                        &cancellation,
                                        &output,
                                    );
                                    return;
                                } else {
                                    record_encoded_packet_submitted(
                                        &shared,
                                        generation,
                                        &output,
                                        submitted_end,
                                        source_end.map(|sample| SourcePosition {
                                            sample,
                                            estimated: packet.source_position_estimated,
                                        }),
                                    );
                                }
                            } else {
                                tracing::debug!(
                                    name: "core.playback.opus_packet_passthrough_rejected",
                                    target: "remedia_core::playback",
                                    playback_id,
                                    packet_bytes = packet.packet.size(),
                                    reason = if packet.has_trimming_metadata { "packet requires container trimming" } else { "packet duration or payload rejected by output" },
                                    "falling back to PCM decoding for this source"
                                );
                                decoder.queue_compressed_packet(packet);
                                can_passthrough = false;
                                lock(&shared.data).passthrough_available = false;
                            }
                        }
                        Ok(None) => input_finished = true,
                        Err(error) => {
                            if is_stopped(&shared, generation) {
                                return;
                            }
                            fail_session(&shared, generation, error.into(), &cancellation, &output);
                            return;
                        }
                    }
                } else {
                    match decoder.next_timed_frame() {
                        Ok(Some(frame)) => pending_frame = Some(frame),
                        Ok(None) => input_finished = true,
                        Err(error) => {
                            if is_stopped(&shared, generation) {
                                return;
                            }
                            fail_session(&shared, generation, error.into(), &cancellation, &output);
                            return;
                        }
                    }
                }
            }
            WorkerAction::Deliver(decoded_frame) => {
                let gain = lock(&shared.data).volume;
                let DecodedPcmFrame {
                    frame,
                    source_start_sample,
                    source_position_estimated,
                } = decoded_frame;
                match processor.write_frame(
                    frame,
                    source_start_sample,
                    source_position_estimated,
                    gain,
                    |frame, source_position| {
                        deliver_processed_frame(
                            &shared,
                            generation,
                            &output,
                            &cancellation,
                            frame,
                            source_position,
                        )
                    },
                ) {
                    Ok(()) => {}
                    Err(ProcessError::Output(_)) if is_stopped(&shared, generation) => return,
                    Err(ProcessError::Output(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                    Err(ProcessError::Filter(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::FilterGraph(Arc::new(error)),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
            }
            WorkerAction::ReplaceFilter(update) => {
                let next_sample = processor.next_sample();
                let should_switch_mode = {
                    let data = lock(&shared.data);
                    let wants_passthrough =
                        can_passthrough && update.graph.is_none() && data.volume == 1.0;
                    let wants_transcoding = update.graph.is_some() || data.volume != 1.0;
                    (passthrough_mode && wants_transcoding)
                        || (!passthrough_mode && wants_passthrough)
                };
                let apply_while_paused = should_switch_mode
                    && !passthrough_mode
                    && lock(&shared.data).state == PlaybackState::Paused
                    && (pending_frame.is_some() || decoder.has_pending_frames());
                match output.filter_transition(next_sample, &cancellation) {
                    Ok(()) => {
                        if should_switch_mode && !apply_while_paused {
                            deferred_filter_update = Some(update);
                        } else {
                            processor.replace(update.graph);
                            let _ = update.response.send(Ok(()));
                        }
                        tracing::info!(
                            name: "core.playback.filter_graph_replaced",
                            target: "remedia_core::playback",
                            playback_id,
                            next_sample,
                            graph_enabled = lock(&shared.data).filter_graph.is_some(),
                            "PCM filtergraph replaced without resetting the output timeline"
                        );
                    }
                    Err(_) if is_stopped(&shared, generation) => {
                        let _ = update.response.send(Err(PlaybackError::Cancelled));
                        return;
                    }
                    Err(error) => {
                        let playback_error = PlaybackError::Output(error);
                        tracing::warn!(
                            name: "core.playback.filter_graph_replacement_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            next_sample,
                            error_kind = "output_transition",
                            "output rejected filtergraph transition"
                        );
                        let _ = update.response.send(Err(playback_error.clone()));
                        fail_session(&shared, generation, playback_error, &cancellation, &output);
                        return;
                    }
                }
            }
            WorkerAction::Pause => match output.pause(&cancellation) {
                Ok(()) => {
                    let mut data = lock(&shared.data);
                    let paused = data.generation == generation
                        && !data.stopping
                        && data.state == PlaybackState::Pausing;
                    if paused {
                        data.state = PlaybackState::Paused;
                    }
                    shared.changed.notify_all();
                    drop(data);
                    if paused {
                        tracing::info!(
                            name: "core.playback.paused",
                            target: "remedia_core::playback",
                            playback_id,
                            "playback paused at frame boundary"
                        );
                    }
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
            WorkerAction::Resume => match output.resume(&cancellation) {
                Ok(()) => {
                    let mut data = lock(&shared.data);
                    let resumed = data.generation == generation
                        && !data.stopping
                        && data.state == PlaybackState::Resuming;
                    if resumed {
                        data.state = PlaybackState::Playing;
                    }
                    shared.changed.notify_all();
                    drop(data);
                    if resumed {
                        tracing::info!(
                            name: "core.playback.resumed",
                            target: "remedia_core::playback",
                            playback_id,
                            "playback resumed"
                        );
                    }
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
            WorkerAction::Seek(request) => {
                if passthrough_mode {
                    if let Err(error) = decoder.activate_decoder(&[], None) {
                        let playback_error: PlaybackError = error.into();
                        let _ = request.response.send(Err(playback_error.clone()));
                        fail_session(&shared, generation, playback_error, &cancellation, &output);
                        return;
                    }
                    passthrough_mode = false;
                }
                // Packet passthrough cannot trim the first packet after a
                // demuxer seek to an arbitrary sample. Keep this source in the
                // decoder path after seeking so FFmpeg can discard pre-target
                // samples exactly.
                can_passthrough = false;
                {
                    let mut data = lock(&shared.data);
                    data.passthrough_available = false;
                    data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
                    shared.changed.notify_all();
                }
                match decoder.seek(request.position) {
                    Ok(()) => {
                        pending_frame = None;
                        input_finished = false;
                        let target_sample = samples_from_duration(request.position);
                        if let Err(error) = processor.reset(target_sample) {
                            let playback_error = PlaybackError::FilterGraph(Arc::new(error));
                            let _ = request.response.send(Err(playback_error.clone()));
                            fail_session(
                                &shared,
                                generation,
                                playback_error,
                                &cancellation,
                                &output,
                            );
                            return;
                        }
                        match output.flush_samples(target_sample, &cancellation) {
                            Ok(()) => {
                                let played = safe_played_position(&output);
                                let mut data = lock(&shared.data);
                                if data.generation == generation && !data.stopping {
                                    data.submitted_sample = target_sample;
                                    data.submitted_position = request.position;
                                    data.media_position = Some(MediaPosition {
                                        position: request.position,
                                        accuracy: MediaPositionAccuracy::Estimated,
                                    });
                                    data.played_position = played;
                                }
                                let _ = request.response.send(Ok(()));
                                tracing::info!(
                                    name: "core.playback.seek_completed",
                                    target: "remedia_core::playback",
                                    playback_id,
                                    position_ms = request.position.as_millis() as u64,
                                    "playback seek applied"
                                );
                            }
                            Err(_) if is_stopped(&shared, generation) => {
                                let _ = request.response.send(Err(PlaybackError::Cancelled));
                                return;
                            }
                            Err(error) => {
                                let playback_error = PlaybackError::Output(error);
                                tracing::error!(
                                    name: "core.playback.seek_failed",
                                    target: "remedia_core::playback",
                                    playback_id,
                                    error_kind = playback_error_kind(&playback_error),
                                    "playback seek failed in output"
                                );
                                let _ = request.response.send(Err(playback_error.clone()));
                                fail_session(
                                    &shared,
                                    generation,
                                    playback_error,
                                    &cancellation,
                                    &output,
                                );
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        if is_stopped(&shared, generation) {
                            let _ = request.response.send(Err(PlaybackError::Cancelled));
                            return;
                        }
                        let playback_error: PlaybackError = error.into();
                        let recoverable = matches!(
                            playback_error,
                            PlaybackError::UnsupportedSeeking(_)
                                | PlaybackError::SeekOutOfRange { .. }
                        );
                        tracing::warn!(
                            name: "core.playback.seek_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            error_kind = playback_error_kind(&playback_error),
                            recoverable,
                            "playback seek was not applied"
                        );
                        let _ = request.response.send(Err(playback_error.clone()));
                        if !recoverable && !is_stopped(&shared, generation) {
                            fail_session(
                                &shared,
                                generation,
                                playback_error,
                                &cancellation,
                                &output,
                            );
                            return;
                        }
                        if is_stopped(&shared, generation) {
                            return;
                        }
                    }
                }
            }
            WorkerAction::Finish => {
                let gain = lock(&shared.data).volume;
                match processor.finish(gain, |frame, source_position| {
                    deliver_processed_frame(
                        &shared,
                        generation,
                        &output,
                        &cancellation,
                        frame,
                        source_position,
                    )
                }) {
                    Ok(()) => {}
                    Err(ProcessError::Output(_)) if is_stopped(&shared, generation) => return,
                    Err(ProcessError::Output(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                    Err(ProcessError::Filter(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::FilterGraph(Arc::new(error)),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
                match output.finish(&cancellation) {
                    Ok(()) => {
                        let played = safe_played_position(&output);
                        let (pending_seek, pending_filter) = {
                            let mut data = lock(&shared.data);
                            let pending = if data.generation == generation && !data.stopping {
                                data.played_position = played.or(data.played_position);
                                data.state = PlaybackState::Completed;
                                (data.pending_seek.take(), data.pending_filter_graph.take())
                            } else {
                                (None, None)
                            };
                            shared.changed.notify_all();
                            pending
                        };
                        if let Some(request) = pending_seek {
                            let _ = request.response.send(Err(PlaybackError::InvalidState {
                                operation: "seek",
                                state: PlaybackState::Completed,
                            }));
                        }
                        if let Some(update) = pending_filter {
                            // The filter setting was committed to the session before EOF won
                            // the worker race. There is no remaining PCM boundary to apply it
                            // to, so acknowledge it for the next source on this session.
                            let graph_enabled = update.graph.is_some();
                            let _ = update.response.send(Ok(()));
                            tracing::info!(
                                name: "core.playback.filter_graph_deferred",
                                target: "remedia_core::playback",
                                playback_id,
                                graph_enabled,
                                "playback completed before filter transition; configuration remains for the next source"
                            );
                        }
                        shared.observability.metrics().playback_completed();
                        tracing::info!(
                            name: "core.playback.completed",
                            target: "remedia_core::playback",
                            playback_id,
                            "playback reached end of stream"
                        );
                        return;
                    }
                    Err(_) if is_stopped(&shared, generation) => return,
                    Err(error) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
            }
        }
    }
}

fn wait_for_start(shared: &SessionShared, generation: u64) -> bool {
    let mut data = lock(&shared.data);
    while data.generation == generation && data.state == PlaybackState::Starting {
        data = wait(&shared.changed, data);
    }
    data.generation == generation && data.state == PlaybackState::Playing
}

fn encoded_packet_from_demuxed(
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

fn next_worker_action(
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

fn fail_session(
    shared: &SessionShared,
    generation: u64,
    error: PlaybackError,
    cancellation: &CancellationToken,
    output: &Arc<dyn MediaOutput>,
) {
    if is_stopped(shared, generation) {
        return;
    }

    cancellation.cancel();
    let _ = safe_cancel(output);
    let (pending_seek, pending_filter, playback_id, output_sample) = {
        let mut data = lock(&shared.data);
        if data.generation != generation
            || matches!(
                data.state,
                PlaybackState::Stopped | PlaybackState::Completed
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
    tracing::error!(
        name: "core.playback.failed",
        target: "remedia_core::playback",
        playback_id,
        output_sample,
        error_kind = playback_error_kind(&error),
        "playback worker failed"
    );
    if let Some(request) = pending_seek {
        let _ = request.response.send(Err(error.clone()));
    }
    if let Some(update) = pending_filter {
        let _ = update.response.send(Err(error));
    }
}

fn deliver_processed_frame(
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

fn record_encoded_packet_submitted(
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

struct ActivePlaybackGuard {
    metrics: remedia_observability::RemediaMetrics,
}

impl Drop for ActivePlaybackGuard {
    fn drop(&mut self) {
        self.metrics.playback_stopped();
    }
}

fn playback_error_kind(error: &PlaybackError) -> &'static str {
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
    }
}

fn is_stopped(shared: &SessionShared, generation: u64) -> bool {
    let data = lock(&shared.data);
    data.generation != generation || data.stopping || data.state == PlaybackState::Stopped
}

fn is_active(state: PlaybackState) -> bool {
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

fn duration_from_samples(samples: u64) -> Duration {
    let rate = u64::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ);
    let seconds = samples / rate;
    let remainder = samples % rate;
    let nanoseconds = (u128::from(remainder) * 1_000_000_000 / u128::from(rate)) as u32;
    Duration::new(seconds, nanoseconds)
}

fn samples_from_duration(position: Duration) -> u64 {
    u64::try_from(
        position.as_nanos() * u128::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000,
    )
    .unwrap_or(u64::MAX)
}

fn normalize_position(position: Duration) -> Result<Duration, PlaybackError> {
    let sample_position =
        position.as_nanos() * u128::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    let sample_position = u64::try_from(sample_position).map_err(|_| {
        PlaybackError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
    })?;
    Ok(duration_from_samples(sample_position))
}

fn safe_cancel(output: &Arc<dyn MediaOutput>) -> Result<(), ()> {
    catch_unwind(AssertUnwindSafe(|| output.cancel())).map_err(|_| ())
}

fn safe_played_position(output: &Arc<dyn MediaOutput>) -> Option<Duration> {
    catch_unwind(AssertUnwindSafe(|| output.played_position()))
        .ok()
        .flatten()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use remedia_observability::{Observability, ObservabilityConfig};

    use super::{
        MediaPositionAccuracy, PlaybackError, PlaybackProcessingMode, PlaybackSession,
        PlaybackState, SessionData, SessionShared, lock, wait,
    };
    use crate::decoder::{AudioDecoder, DecoderOptions, PcmFrame};
    use crate::opus::{EncodedOpusPacket, OpusEncoderOutput, OpusPacketSink};
    use crate::output::{MediaOutput, NullOutput, NullOutputStats, OutputError};
    use crate::source::{CancellationToken, InputSource};

    static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);
    // Playback worker tests coordinate several threads and native FFmpeg
    // operations. Leave enough room for loaded hosted runners while retaining
    // a finite bound so genuine deadlocks still fail deterministically.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    struct TempWave {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TempWave {
        fn constant(sample_count: usize, sample: i16) -> Self {
            Self::from_samples(vec![sample; sample_count])
        }

        fn from_samples(samples: Vec<i16>) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let directory =
                std::env::temp_dir().join(format!("remedia-playback-{}-{id}", std::process::id()));
            fs::create_dir_all(&directory).expect("create playback test directory");
            let path = directory.join("source.wav");
            fs::write(&path, pcm_wave(&samples)).expect("write playback fixture");
            Self { directory, path }
        }

        fn source(&self) -> InputSource {
            InputSource::file(&self.path)
        }
    }

    impl Drop for TempWave {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[derive(Clone)]
    struct GateOutput(Arc<GateShared>);

    #[derive(Clone)]
    struct GateHandle(Arc<GateShared>);

    struct GateShared {
        data: Mutex<GateData>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct GateData {
        permits: usize,
        write_calls: usize,
        accepted: Vec<(u64, f32)>,
        flushes: Vec<Duration>,
        position: Duration,
        cancelled: bool,
        finish_blocked: bool,
        finish_started: bool,
        finished: bool,
    }

    impl GateOutput {
        fn new() -> (Self, GateHandle) {
            let shared = Arc::new(GateShared {
                data: Mutex::new(GateData::default()),
                changed: Condvar::new(),
            });
            (Self(Arc::clone(&shared)), GateHandle(shared))
        }

        fn blocked_finish() -> (Self, GateHandle) {
            let (output, handle) = Self::new();
            lock(&handle.0.data).finish_blocked = true;
            (output, handle)
        }
    }

    impl GateHandle {
        fn release(&self, count: usize) {
            let mut data = lock(&self.0.data);
            data.permits = data.permits.saturating_add(count);
            self.0.changed.notify_all();
        }

        fn wait_for_calls(&self, count: usize) -> bool {
            let data = lock(&self.0.data);
            let (data, _) = self
                .0
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.write_calls < count && !data.cancelled
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.write_calls >= count
        }

        fn wait_for_accepted(&self, count: usize) -> bool {
            let data = lock(&self.0.data);
            let (data, _) = self
                .0
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.accepted.len() < count && !data.cancelled
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.accepted.len() >= count
        }

        fn wait_for_finish(&self) -> bool {
            let data = lock(&self.0.data);
            let (data, _) = self
                .0
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    !data.finish_started && !data.cancelled
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.finish_started
        }

        fn release_finish(&self) {
            let mut data = lock(&self.0.data);
            data.finish_blocked = false;
            self.0.changed.notify_all();
        }

        fn snapshot(&self) -> GateSnapshot {
            let data = lock(&self.0.data);
            GateSnapshot {
                write_calls: data.write_calls,
                accepted: data.accepted.clone(),
                flushes: data.flushes.clone(),
                cancelled: data.cancelled,
                finished: data.finished,
            }
        }
    }

    #[derive(Debug)]
    struct GateSnapshot {
        write_calls: usize,
        accepted: Vec<(u64, f32)>,
        flushes: Vec<Duration>,
        cancelled: bool,
        finished: bool,
    }

    impl MediaOutput for GateOutput {
        fn write_frame(
            &self,
            frame: PcmFrame,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            let mut data = lock(&self.0.data);
            data.write_calls += 1;
            self.0.changed.notify_all();
            while data.permits == 0 && !data.cancelled && !cancellation.is_cancelled() {
                data = wait(&self.0.changed, data);
            }
            if data.cancelled || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            data.permits -= 1;
            let first_sample = frame.samples.first().copied().unwrap_or_default();
            data.position = samples_to_duration(
                frame
                    .start_sample
                    .saturating_add(frame.samples_per_channel as u64),
            );
            data.accepted.push((frame.start_sample, first_sample));
            self.0.changed.notify_all();
            Ok(())
        }

        fn flush(
            &self,
            position: Duration,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let mut data = lock(&self.0.data);
            if data.cancelled {
                return Err(OutputError::Cancelled);
            }
            data.position = position;
            data.flushes.push(position);
            self.0.changed.notify_all();
            Ok(())
        }

        fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
            let mut data = lock(&self.0.data);
            data.finish_started = true;
            self.0.changed.notify_all();
            while data.finish_blocked && !data.cancelled && !cancellation.is_cancelled() {
                data = wait(&self.0.changed, data);
            }
            if data.cancelled || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            data.finished = true;
            self.0.changed.notify_all();
            Ok(())
        }

        fn cancel(&self) {
            let mut data = lock(&self.0.data);
            data.cancelled = true;
            self.0.changed.notify_all();
        }

        fn played_position(&self) -> Option<Duration> {
            Some(lock(&self.0.data).position)
        }
    }

    struct FailingOutput(Arc<AtomicBool>);

    impl MediaOutput for FailingOutput {
        fn write_frame(
            &self,
            _frame: PcmFrame,
            _cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            Err(OutputError::new("controlled output failure"))
        }

        fn flush(
            &self,
            _position: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            Ok(())
        }

        fn finish(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
            Ok(())
        }

        fn cancel(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[derive(Clone)]
    struct StepPacketSink(Arc<StepSinkShared>, bool);

    #[derive(Default)]
    struct StepSinkShared {
        data: Mutex<StepSinkData>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct StepSinkData {
        permits: usize,
        calls: usize,
        packets: Vec<EncodedOpusPacket>,
        flushes: usize,
        finished: bool,
        cancelled: bool,
    }

    #[derive(Clone)]
    struct StepPacketHandle(Arc<StepSinkShared>);

    impl StepPacketSink {
        fn new() -> (Self, StepPacketHandle) {
            Self::with_passthrough(true)
        }

        fn without_passthrough() -> (Self, StepPacketHandle) {
            Self::with_passthrough(false)
        }

        fn with_passthrough(passthrough_supported: bool) -> (Self, StepPacketHandle) {
            let shared = Arc::new(StepSinkShared::default());
            (
                Self(Arc::clone(&shared), passthrough_supported),
                StepPacketHandle(shared),
            )
        }
    }

    impl StepPacketHandle {
        fn calls(&self) -> usize {
            lock(&self.0.data).calls
        }

        fn release(&self, count: usize) {
            let mut data = lock(&self.0.data);
            data.permits = data.permits.saturating_add(count);
            self.0.changed.notify_all();
        }

        fn wait_for_calls(&self, count: usize) -> bool {
            let data = lock(&self.0.data);
            let (data, _) = self
                .0
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.calls < count && !data.cancelled
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.calls >= count
        }

        fn wait_for_pending_filter(&self, session: &PlaybackSession) -> bool {
            let data = lock(&session.shared.data);
            let (data, _) = session
                .shared
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.pending_filter_graph.is_none()
                        && !matches!(data.state, PlaybackState::Stopped | PlaybackState::Failed)
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.pending_filter_graph.is_some()
        }

        fn wait_for_pending_seek(&self, session: &PlaybackSession) -> bool {
            let data = lock(&session.shared.data);
            let (data, _) = session
                .shared
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.pending_seek.is_none()
                        && !matches!(data.state, PlaybackState::Stopped | PlaybackState::Failed)
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.pending_seek.is_some()
        }

        fn snapshot(&self) -> StepSinkData {
            let data = lock(&self.0.data);
            StepSinkData {
                permits: data.permits,
                calls: data.calls,
                packets: data.packets.clone(),
                flushes: data.flushes,
                finished: data.finished,
                cancelled: data.cancelled,
            }
        }
    }

    impl OpusPacketSink for StepPacketSink {
        fn supports_passthrough_packet(&self, _packet: &EncodedOpusPacket) -> bool {
            self.1
        }

        fn send_packet(
            &self,
            packet: EncodedOpusPacket,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            let mut data = lock(&self.0.data);
            data.calls += 1;
            self.0.changed.notify_all();
            while data.permits == 0 && !data.cancelled && !cancellation.is_cancelled() {
                data = self
                    .0
                    .changed
                    .wait(data)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if data.cancelled || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            data.permits -= 1;
            data.packets.push(packet);
            Ok(())
        }

        fn flush(
            &self,
            _position: Duration,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            lock(&self.0.data).flushes += 1;
            Ok(())
        }

        fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            lock(&self.0.data).finished = true;
            Ok(())
        }

        fn cancel(&self) {
            let mut data = lock(&self.0.data);
            data.cancelled = true;
            self.0.changed.notify_all();
        }
    }

    #[test]
    fn compatible_opus_source_is_forwarded_without_transcoding() {
        let bytes = include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg").to_vec();
        let source = InputSource::bytes(bytes.clone());
        let mut packet_reader =
            AudioDecoder::open_for_playback(source.clone(), DecoderOptions::default()).unwrap();
        assert_eq!(packet_reader.opus_passthrough_rejection(), None);
        let mut expected_payloads = Vec::new();
        while let Some(packet) = packet_reader.read_compressed_packet().unwrap() {
            expected_payloads.push(packet.packet.data().unwrap().to_vec());
        }
        let preroll = packet_reader.packet_history_before(u64::MAX);
        assert_eq!(preroll.len(), 6, "packet history is capped at 120 ms");

        let (sink, handle) = StepPacketSink::new();
        handle.release(1_000);
        let output = OpusEncoderOutput::new(sink).unwrap();
        let session = PlaybackSession::new();
        let info = session.play(source, output).unwrap();
        assert_eq!(info.codec, "opus");
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough)
        );
        assert_eq!(session.wait(), PlaybackState::Completed);

        let packets = handle.snapshot().packets;
        assert_eq!(packets.len(), expected_payloads.len());
        assert_eq!(
            packets
                .iter()
                .map(|packet| packet.bytes.clone())
                .collect::<Vec<_>>(),
            expected_payloads,
            "passthrough must preserve each demuxed Opus payload"
        );
        assert!(
            packets
                .iter()
                .all(|packet| packet.samples_per_channel == 960)
        );
        assert!(packets.iter().enumerate().all(|(index, packet)| {
            packet.start_sample == index as u64 * 960
                && packet.media_samples_per_channel == packet.samples_per_channel
        }));
        assert_eq!(session.position().submitted, Duration::from_millis(2_020));
        assert_eq!(handle.snapshot().flushes, 0);
    }

    #[test]
    fn non_opus_stream_falls_back_to_pcm_and_native_opus_encoding() {
        let source_bytes = include_bytes!("../tests/fixtures/generated-tone.mp3").to_vec();
        let reader = AudioDecoder::open_for_playback(
            InputSource::bytes(source_bytes.clone()),
            DecoderOptions::default(),
        )
        .unwrap();
        assert_eq!(
            reader.opus_passthrough_rejection(),
            Some("selected codec is not Opus")
        );

        let (sink, handle) = StepPacketSink::new();
        handle.release(1_000);
        let session = PlaybackSession::new();
        session
            .play(
                InputSource::bytes(source_bytes),
                OpusEncoderOutput::new(sink).unwrap(),
            )
            .unwrap();
        assert_eq!(session.wait(), PlaybackState::Completed);
        let packets = handle.snapshot().packets;
        assert!(!packets.is_empty());
        assert!(
            packets
                .iter()
                .all(|packet| packet.samples_per_channel == 960)
        );
        assert!(packets.windows(2).all(|pair| {
            pair[0]
                .start_sample
                .saturating_add(u64::from(pair[0].media_samples_per_channel))
                == pair[1].start_sample
        }));
    }

    #[test]
    fn opus_with_encoder_preskip_falls_back_instead_of_forwarding_trimmed_packets() {
        let source_bytes = include_bytes!("../tests/fixtures/generated-opus-trimmed.ogg").to_vec();
        let reader = AudioDecoder::open_for_playback(
            InputSource::bytes(source_bytes.clone()),
            DecoderOptions::default(),
        )
        .unwrap();
        assert!(reader.opus_passthrough_rejection().is_some());

        let (sink, handle) = StepPacketSink::new();
        handle.release(1_000);
        let session = PlaybackSession::new();
        session
            .play(
                InputSource::bytes(source_bytes),
                OpusEncoderOutput::new(sink).unwrap(),
            )
            .unwrap();
        assert_eq!(session.wait(), PlaybackState::Completed);
        assert!(!handle.snapshot().packets.is_empty());
    }

    #[test]
    fn output_packet_policy_can_reject_passthrough_and_fall_back_to_pcm() {
        let (sink, handle) = StepPacketSink::without_passthrough();
        handle.release(1_000);
        let session = PlaybackSession::new();
        session
            .play(
                InputSource::bytes(
                    include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
                ),
                OpusEncoderOutput::new(sink).unwrap(),
            )
            .unwrap();

        assert_eq!(session.wait(), PlaybackState::Completed);
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Transcoding)
        );
        assert!(!handle.snapshot().packets.is_empty());
    }

    #[test]
    fn concurrent_opus_sessions_keep_processing_modes_independent() {
        let fixture = include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg");
        let (passthrough_sink, passthrough_handle) = StepPacketSink::new();
        let (processing_sink, processing_handle) = StepPacketSink::new();
        passthrough_handle.release(1_000);
        processing_handle.release(1_000);
        let passthrough = PlaybackSession::new();
        let processing = PlaybackSession::new();
        processing.set_volume(0.75).unwrap();

        passthrough
            .play(
                InputSource::bytes(fixture.to_vec()),
                OpusEncoderOutput::new(passthrough_sink).unwrap(),
            )
            .unwrap();
        processing
            .play(
                InputSource::bytes(fixture.to_vec()),
                OpusEncoderOutput::new(processing_sink).unwrap(),
            )
            .unwrap();

        assert_eq!(
            passthrough.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough)
        );
        assert_eq!(
            processing.processing_mode(),
            Some(PlaybackProcessingMode::Transcoding)
        );
        assert_eq!(passthrough.wait(), PlaybackState::Completed);
        assert_eq!(processing.wait(), PlaybackState::Completed);
        assert_eq!(
            passthrough.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough)
        );
        assert_eq!(
            processing.processing_mode(),
            Some(PlaybackProcessingMode::Transcoding)
        );
        assert!(!passthrough_handle.snapshot().packets.is_empty());
        assert!(!processing_handle.snapshot().packets.is_empty());
    }

    #[test]
    fn passthrough_to_pcm_can_be_staged_while_paused() {
        let (sink, handle) = StepPacketSink::new();
        let session = Arc::new(PlaybackSession::new());
        session
            .play(
                InputSource::bytes(
                    include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
                ),
                OpusEncoderOutput::new(sink).unwrap(),
            )
            .unwrap();
        assert!(handle.wait_for_calls(1));

        let pause_session = Arc::clone(&session);
        let (pause_tx, pause_rx) = mpsc::sync_channel(1);
        let pause_worker = thread::spawn(move || {
            let _ = pause_tx.send(pause_session.pause());
        });
        assert!(wait_for_state(&session, PlaybackState::Pausing));
        handle.release(1);
        pause_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        pause_worker.join().unwrap();
        assert_eq!(session.state(), PlaybackState::Paused);

        session.set_volume(0.75).unwrap();
        assert!(wait_for_processing_mode(
            &session,
            PlaybackProcessingMode::Transcoding
        ));
        assert_eq!(session.state(), PlaybackState::Paused);

        session.resume().unwrap();
        handle.release(1_000);
        assert_eq!(session.wait(), PlaybackState::Completed);
        assert!(!handle.snapshot().packets.is_empty());
    }

    #[test]
    fn seeking_during_passthrough_uses_pcm_trimming_and_invalidates_old_packets() {
        let (sink, handle) = StepPacketSink::new();
        let session = Arc::new(PlaybackSession::new());
        session
            .play(
                InputSource::bytes(
                    include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
                ),
                OpusEncoderOutput::new(sink).unwrap(),
            )
            .unwrap();
        assert!(handle.wait_for_calls(1));

        let seeking_session = Arc::clone(&session);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let seeker = thread::spawn(move || {
            let _ = response_tx.send(seeking_session.seek(Duration::from_millis(500)));
        });
        assert!(handle.wait_for_pending_seek(&session));
        handle.release(1);
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        seeker.join().unwrap();

        assert!(
            handle.wait_for_calls(2),
            "seek falls back to decoded output"
        );
        handle.release(1_000);
        assert_eq!(session.wait(), PlaybackState::Completed);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.flushes, 1);
        assert!(
            snapshot
                .packets
                .iter()
                .any(|packet| packet.start_sample >= 24_000)
        );
    }

    #[test]
    fn one_opus_source_switches_between_passthrough_and_live_filtering() {
        let fixture = include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg");
        let (source, producer) = InputSource::byte_stream(1_024).unwrap();
        let source_bytes = fixture.to_vec();
        let producer_thread = thread::spawn(move || {
            for chunk in source_bytes.chunks(317) {
                producer.write(chunk).unwrap();
            }
            producer.finish().unwrap();
        });
        let (sink, handle) = StepPacketSink::new();
        let output = OpusEncoderOutput::new(sink).unwrap();
        let session = Arc::new(PlaybackSession::new());
        session.play(source, output).expect("Opus source starts");

        assert!(handle.wait_for_calls(1));
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough)
        );
        let update_session = Arc::clone(&session);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let first_update = thread::spawn(move || {
            let _ = response_tx.send(update_session.set_filter_graph(Some("bass=g=8")));
        });
        assert!(handle.wait_for_pending_filter(&session));
        handle.release(1);
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        first_update.join().unwrap();
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Transcoding),
            "filter update resolves only after PCM mode is active"
        );

        assert!(
            handle.wait_for_calls(2),
            "PCM transcoding reaches the same sink"
        );
        let update_session = Arc::clone(&session);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let second_update = thread::spawn(move || {
            let _ = response_tx.send(update_session.set_filter_graph(Some("atempo=1.05")));
        });
        assert!(handle.wait_for_pending_filter(&session));
        handle.release(1);
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        second_update.join().unwrap();

        assert!(handle.wait_for_calls(3), "tempo graph remains active");
        let update_session = Arc::clone(&session);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let third_update = thread::spawn(move || {
            let _ = response_tx.send(update_session.set_filter_graph(Some("tremolo=f=5:d=0.6")));
        });
        assert!(handle.wait_for_pending_filter(&session));
        handle.release(1);
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        third_update.join().unwrap();

        assert!(handle.wait_for_calls(4), "replacement graph remains active");
        replace_graph_while_packet_sender_is_blocked(Arc::clone(&session), handle.clone(), None);
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough),
            "clear resolves only after passthrough mode is active"
        );

        assert!(handle.wait_for_calls(5), "clear returns to passthrough");
        let update_session = Arc::clone(&session);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let set_gain = thread::spawn(move || {
            let _ = response_tx.send(update_session.set_volume(0.75));
        });
        assert!(wait_for_volume(&session, 0.75));
        handle.release(1);
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        set_gain.join().unwrap();
        assert!(
            handle.wait_for_calls(6),
            "non-unity gain reactivates PCM processing"
        );
        let update_session = Arc::clone(&session);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let restore_gain = thread::spawn(move || {
            let _ = response_tx.send(update_session.set_volume(1.0));
        });
        handle.release(1_000);
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        restore_gain.join().unwrap();
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough),
            "unity gain returns to passthrough"
        );
        assert_eq!(session.wait(), PlaybackState::Completed);
        producer_thread.join().unwrap();
        let snapshot = handle.snapshot();
        assert!(snapshot.finished);
        assert_eq!(snapshot.flushes, 0, "mode changes must not seek-flush RTP");
        assert_eq!(session.filter_graph(), None);
        assert_eq!(session.volume(), 1.0);
        assert!(
            snapshot
                .packets
                .iter()
                .any(|packet| packet.start_sample > 0)
        );
        assert!(snapshot.packets.windows(2).all(|pair| {
            pair[0]
                .start_sample
                .saturating_add(u64::from(pair[0].media_samples_per_channel))
                == pair[1].start_sample
        }));

        let mut source_reader = AudioDecoder::open_for_playback(
            InputSource::bytes(fixture.to_vec()),
            DecoderOptions::default(),
        )
        .unwrap();
        let mut source_payloads = Vec::new();
        while let Some(packet) = source_reader.read_compressed_packet().unwrap() {
            source_payloads.push(packet.packet.data().unwrap().to_vec());
        }
        assert_eq!(
            snapshot.packets.last().map(|packet| &packet.bytes),
            source_payloads.last(),
            "the final packet returned to passthrough on the same source"
        );
        assert_eq!(
            snapshot.packets.first().map(|packet| &packet.bytes),
            source_payloads.first()
        );
        assert!(snapshot.packets.iter().any(|packet| {
            source_payloads
                .iter()
                .all(|source_payload| source_payload != &packet.bytes)
        }));
    }

    #[test]
    fn null_output_reports_natural_completion_and_position() {
        let source = TempWave::constant(4800, 4096);
        let session = PlaybackSession::new();
        let output = NullOutput::new();
        let stats = output.stats_handle();

        let info = session.play(source.source(), output).unwrap();
        assert_eq!(info.duration, Some(Duration::from_millis(100)));
        assert_eq!(session.wait(), PlaybackState::Completed);

        let output_stats: NullOutputStats = stats.snapshot();
        assert!(output_stats.frames_received > 0);
        assert_eq!(output_stats.samples_received, 9600);
        assert_eq!(output_stats.sample_frames_received, 4800);
        assert_eq!(output_stats.duration, Duration::from_millis(100));
        assert!(output_stats.finished_successfully);
        assert_eq!(session.duration(), Some(Duration::from_millis(100)));
        assert_eq!(session.position().submitted, Duration::from_millis(100));
        assert_eq!(session.position().played, Some(Duration::from_millis(100)));
        assert_eq!(
            session.position().media.map(|position| position.position),
            Some(Duration::from_millis(100))
        );
        session.stop().unwrap();
        assert_eq!(session.state(), PlaybackState::Completed);
    }

    #[test]
    fn source_media_position_survives_tempo_and_speed_filters() {
        for (graph, expected_output_samples) in [
            ("volume=0.5", 48_000),
            ("asetrate=60000,aresample=48000", 38_400),
            ("asetrate=38400,aresample=48000", 60_000),
            ("atempo=1.25", 38_400),
        ] {
            let source = TempWave::constant(48_000, 4_096);
            let session = PlaybackSession::new();
            session.set_filter_graph(Some(graph)).unwrap();
            let output = NullOutput::new();
            session.play(source.source(), output).unwrap();
            assert_eq!(session.wait(), PlaybackState::Completed, "{graph}");

            let position = session.position();
            assert_eq!(
                position.media.map(|media| media.position),
                Some(Duration::from_secs(1))
            );
            assert_eq!(
                position.media.map(|media| media.accuracy),
                Some(MediaPositionAccuracy::Estimated)
            );
            assert!(
                (position.submitted.as_millis() as i64
                    - expected_output_samples as i64 * 1_000 / 48_000)
                    .abs()
                    <= 3,
                "{graph}: {position:?}"
            );
            session.stop().unwrap();
        }
    }

    #[test]
    fn mp3_skip_and_discard_samples_keep_the_media_position_near_track_end() {
        let session = PlaybackSession::new();
        let info = session
            .play(
                InputSource::bytes(include_bytes!("../tests/fixtures/generated-tone.mp3").to_vec()),
                NullOutput::new(),
            )
            .expect("bundled MP3 opens");
        assert_eq!(session.wait(), PlaybackState::Completed);
        let expected_duration = info.duration.expect("fixture duration is known");
        let media_position = session
            .position()
            .media
            .expect("decoded frames report their media position")
            .position;
        assert!(
            media_position.abs_diff(expected_duration) < Duration::from_millis(100),
            "final media position {media_position:?}, track duration {expected_duration:?}"
        );
        session.stop().unwrap();
    }

    #[test]
    fn live_filter_replacement_drops_old_graph_delay_and_keeps_master_gain_independent() {
        let source = TempWave::constant(96_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        session.set_volume(0.5).unwrap();
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let update_session = Arc::clone(&session);
        let (updated, updated_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = updated.send(update_session.set_filter_graph(Some("volume=0.25")));
        });
        assert!(wait_for_pending_filter_update(&session));
        session.set_volume(0.0).unwrap();
        gate.release(1);
        let update_result = updated_rx.recv_timeout(TEST_TIMEOUT);
        assert!(
            update_result.is_ok(),
            "filter replacement timed out: state={:?}, error={:?}, output={:?}",
            session.state(),
            session.last_error(),
            gate.snapshot()
        );
        assert!(update_result.unwrap().is_ok());
        assert_eq!(session.filter_graph().as_deref(), Some("volume=0.25"));
        assert!(gate.wait_for_calls(2));
        session.set_volume(1.5).unwrap();
        gate.release(1);
        assert!(gate.wait_for_calls(3));
        gate.release(1);
        assert!(gate.wait_for_accepted(3));

        let snapshot = gate.snapshot();
        let input_level = 4096.0 / 32768.0 / std::f32::consts::SQRT_2;
        assert!((snapshot.accepted[0].1 - input_level * 0.5).abs() < 0.001);
        assert!(snapshot.accepted[1].1.abs() < 0.001);
        assert!((snapshot.accepted[2].1 - input_level * 0.25 * 1.5).abs() < 0.001);
        assert!(snapshot.flushes.is_empty());
        assert_eq!(session.volume(), 1.5);

        assert!(matches!(
            session.set_filter_graph(Some("missing_filter=1")),
            Err(PlaybackError::FilterGraph(_))
        ));
        assert_eq!(session.filter_graph().as_deref(), Some("volume=0.25"));
        session.stop().unwrap();
    }

    #[test]
    fn filter_and_seek_requests_racing_with_eof_are_resolved() {
        let source = TempWave::constant(960, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::blocked_finish();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));
        gate.release(usize::MAX);
        assert!(gate.wait_for_finish());

        let filter_session = Arc::clone(&session);
        let (filter_tx, filter_rx) = mpsc::sync_channel(1);
        let filter_thread = thread::spawn(move || {
            let _ = filter_tx.send(filter_session.set_filter_graph(Some("tremolo=f=5:d=0.6")));
        });
        assert!(wait_for_pending_filter_update(&session));

        let seek_session = Arc::clone(&session);
        let (seek_tx, seek_rx) = mpsc::sync_channel(1);
        let seek_thread = thread::spawn(move || {
            let _ = seek_tx.send(seek_session.seek(Duration::from_millis(5)));
        });
        assert!(wait_for_pending_seek(&session));

        gate.release_finish();
        assert!(
            filter_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("filter config is acknowledged after EOF")
                .is_ok()
        );
        assert!(matches!(
            seek_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("pending seek is resolved after EOF"),
            Err(PlaybackError::InvalidState {
                operation: "seek",
                state: PlaybackState::Completed,
            })
        ));
        filter_thread.join().unwrap();
        seek_thread.join().unwrap();
        assert_eq!(session.state(), PlaybackState::Completed);
        assert_eq!(session.filter_graph().as_deref(), Some("tremolo=f=5:d=0.6"));
        session.stop().unwrap();
    }

    #[test]
    fn native_opus_filter_changes_keep_playback_active_and_packet_timeline_contiguous() {
        let source = TempWave::constant(96_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        session.set_volume(0.75).unwrap();
        let (sink, gate) = StepPacketSink::new();
        session
            .play(
                source.source(),
                OpusEncoderOutput::new(sink).expect("native Opus encoder initializes"),
            )
            .unwrap();
        assert!(
            gate.wait_for_calls(1),
            "Opus output reaches its first packet"
        );

        replace_graph_while_packet_sender_is_blocked(
            Arc::clone(&session),
            gate.clone(),
            Some("tremolo=f=5:d=0.6"),
        );
        assert_eq!(session.state(), PlaybackState::Playing);
        let mut latest_media_position = session
            .position()
            .media
            .expect("first accepted Opus output has a source position")
            .position;
        replace_graph_while_packet_sender_is_blocked(
            Arc::clone(&session),
            gate.clone(),
            Some("atempo=1.25"),
        );
        assert_eq!(session.state(), PlaybackState::Playing);
        let media_position = session
            .position()
            .media
            .expect("tempo output retains its source position")
            .position;
        assert!(media_position >= latest_media_position);
        latest_media_position = media_position;
        replace_graph_while_packet_sender_is_blocked(Arc::clone(&session), gate.clone(), None);
        assert_eq!(session.state(), PlaybackState::Playing);
        assert_eq!(session.volume(), 0.75);
        assert!(
            session
                .position()
                .media
                .expect("cleared graph retains source progress")
                .position
                >= latest_media_position
        );

        gate.release(usize::MAX);
        assert_eq!(session.wait(), PlaybackState::Completed);
        assert_eq!(
            session.position().media.map(|media| media.position),
            Some(Duration::from_secs(2))
        );
        let snapshot = gate.snapshot();
        assert!(snapshot.finished);
        assert_eq!(snapshot.flushes, 0, "filter changes do not seek-flush Opus");
        assert!(snapshot.packets.len() > 10);
        for (index, packet) in snapshot.packets.iter().enumerate() {
            assert_eq!(packet.start_sample, index as u64 * 960);
            assert_eq!(packet.samples_per_channel, 960);
            assert!(packet.media_samples_per_channel <= 960);
        }
        session.stop().unwrap();
    }

    #[test]
    fn stop_cancels_a_pending_native_opus_filter_replacement() {
        let source = TempWave::constant(48_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (sink, gate) = StepPacketSink::new();
        session
            .play(
                source.source(),
                OpusEncoderOutput::new(sink).expect("native Opus encoder initializes"),
            )
            .unwrap();
        assert!(gate.wait_for_calls(1));

        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let update_session = Arc::clone(&session);
        let update_worker = thread::spawn(move || {
            let _ = result_tx.send(update_session.set_filter_graph(Some("tremolo=f=5:d=0.6")));
        });
        assert!(wait_for_pending_filter_update(&session));

        session
            .stop()
            .expect("stop cancels output and joins the worker");
        assert!(matches!(
            result_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
            Err(PlaybackError::Cancelled)
        ));
        update_worker.join().expect("filter update worker joins");
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
    }

    #[test]
    fn filter_replacement_and_seek_are_applied_while_paused() {
        let source = TempWave::constant(48_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let pause_session = Arc::clone(&session);
        let (paused, paused_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = paused.send(pause_session.pause());
        });
        assert!(wait_for_state(&session, PlaybackState::Pausing));
        gate.release(1);
        assert!(
            paused_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("pause should reach a frame boundary")
                .is_ok()
        );
        assert_eq!(session.state(), PlaybackState::Paused);
        let paused_media_position = session.position().media.map(|media| media.position);

        session.set_filter_graph(Some("volume=0.5")).unwrap();
        assert_eq!(session.state(), PlaybackState::Paused);
        assert_eq!(
            session.position().media.map(|media| media.position),
            paused_media_position
        );
        let seek_position = Duration::from_millis(500);
        session.seek(seek_position).unwrap();
        assert_eq!(session.position().submitted, seek_position);
        assert_eq!(
            session.position().media.map(|media| media.position),
            Some(seek_position)
        );
        assert_eq!(
            session.position().media.map(|media| media.accuracy),
            Some(MediaPositionAccuracy::Estimated)
        );
        assert_eq!(gate.snapshot().flushes.last(), Some(&seek_position));

        session.resume().unwrap();
        assert!(gate.wait_for_calls(2));
        gate.release(1);
        assert!(gate.wait_for_accepted(2));
        let snapshot = gate.snapshot();
        let (start_sample, first_sample) = snapshot.accepted[1];
        assert!(start_sample >= 24_000);
        let input_level = 4096.0 / 32768.0 / std::f32::consts::SQRT_2;
        assert!((first_sample - input_level * 0.5).abs() < 0.001);
        session.stop().unwrap();
    }

    #[test]
    fn completion_waiter_remains_bound_to_its_source_after_replacement() {
        let source = TempWave::constant(48_000, 4096);
        let session = PlaybackSession::new();
        let (first_output, first_gate) = GateOutput::new();
        session.play(source.source(), first_output).unwrap();
        assert!(first_gate.wait_for_calls(1));
        let first_waiter = session.completion();

        session.stop().unwrap();
        let (second_output, second_gate) = GateOutput::new();
        session.play(source.source(), second_output).unwrap();
        assert!(second_gate.wait_for_calls(1));

        assert_eq!(first_waiter.wait(), PlaybackState::Stopped);
        assert_eq!(session.state(), PlaybackState::Playing);
        session.stop().unwrap();
    }

    #[test]
    fn pause_resume_stop_and_repeated_operation_transitions_are_typed() {
        let source = TempWave::constant(96_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        assert!(matches!(
            session.pause(),
            Err(PlaybackError::InvalidState { .. })
        ));
        assert!(matches!(
            session.resume(),
            Err(PlaybackError::InvalidState { .. })
        ));
        session.stop().unwrap();
        session.stop().unwrap();
        assert_eq!(session.state(), PlaybackState::Stopped);

        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(matches!(
            session.play(source.source(), NullOutput::new()),
            Err(PlaybackError::InvalidState { .. })
        ));
        pause_at_first_blocked_frame(&session, &gate);
        session.pause().unwrap();
        assert_eq!(session.state(), PlaybackState::Paused);
        session.resume().unwrap();
        session.resume().unwrap();
        assert!(gate.wait_for_calls(2));
        session.stop().unwrap();
        session.stop().unwrap();
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn stop_during_paused_playback_releases_the_worker() {
        let source = TempWave::constant(48_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        pause_at_first_blocked_frame(&session, &gate);

        session.stop().unwrap();

        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn concurrent_stop_calls_wait_for_cleanup_and_both_succeed() {
        let source = TempWave::constant(48_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let barrier = Arc::new(Barrier::new(3));
        let first_session = Arc::clone(&session);
        let first_barrier = Arc::clone(&barrier);
        let first_stop = thread::spawn(move || {
            first_barrier.wait();
            first_session.stop()
        });
        let second_session = Arc::clone(&session);
        let second_barrier = Arc::clone(&barrier);
        let second_stop = thread::spawn(move || {
            second_barrier.wait();
            second_session.stop()
        });

        barrier.wait();
        assert!(first_stop.join().unwrap().is_ok());
        assert!(second_stop.join().unwrap().is_ok());
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn blocked_output_applies_backpressure_without_a_session_audio_queue() {
        let source = TempWave::constant(96_000, 4096);
        let session = PlaybackSession::new();
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let blocked = gate.snapshot();
        assert_eq!(blocked.write_calls, 1);
        assert!(blocked.accepted.is_empty());
        assert_eq!(session.position().submitted, Duration::ZERO);

        session.stop().unwrap();
        let stopped = gate.snapshot();
        assert_eq!(stopped.write_calls, 1);
        assert!(stopped.cancelled);
    }

    #[test]
    fn seeking_while_paused_flushes_output_and_discards_stale_frames() {
        let source = TempWave::from_samples(
            (0..96_000)
                .map(|index| if index < 48_000 { 1200 } else { 12_000 })
                .collect(),
        );
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        pause_at_first_blocked_frame(&session, &gate);

        let target = Duration::from_secs(1);
        session.seek(target).unwrap();
        assert_eq!(session.position().submitted, target);
        assert_eq!(gate.snapshot().flushes, vec![target]);
        let accepted_before_seek_resume = gate.snapshot().accepted.len();

        session.resume().unwrap();
        assert!(gate.wait_for_calls(accepted_before_seek_resume + 1));
        gate.release(1);
        assert!(gate.wait_for_accepted(accepted_before_seek_resume + 1));
        let snapshot = gate.snapshot();
        let (start_sample, first_sample) = snapshot.accepted[accepted_before_seek_resume];
        assert!(start_sample >= 48_000);
        assert!(
            first_sample > 0.2,
            "post-seek audio should come from the second half"
        );

        session.stop().unwrap();
    }

    #[test]
    fn pre_cancelled_play_does_not_start_a_source() {
        let session = PlaybackSession::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = session.play_with_cancellation(
            InputSource::file("missing.wav"),
            NullOutput::new(),
            cancellation,
        );

        assert!(matches!(result, Err(PlaybackError::Cancelled)));
        assert_eq!(session.state(), PlaybackState::Idle);
    }

    #[test]
    fn decoder_and_output_failures_reach_the_session() {
        let invalid_directory =
            std::env::temp_dir().join(format!("remedia-playback-invalid-{}", std::process::id()));
        fs::create_dir_all(&invalid_directory).unwrap();
        let invalid_path = invalid_directory.join("invalid.bin");
        fs::write(&invalid_path, b"not a media file").unwrap();

        let decoder_session = PlaybackSession::new();
        let (decoder_error_output, decoder_output_handle) = GateOutput::new();
        assert!(matches!(
            decoder_session.play(InputSource::file(&invalid_path), decoder_error_output),
            Err(PlaybackError::Decoder(_))
        ));
        assert_eq!(decoder_session.state(), PlaybackState::Failed);
        assert!(decoder_output_handle.snapshot().cancelled);
        assert!(matches!(
            decoder_session.last_error(),
            Some(PlaybackError::Decoder(_))
        ));
        decoder_session.stop().unwrap();
        let _ = fs::remove_dir_all(&invalid_directory);

        let source = TempWave::constant(48_000, 4096);
        let output_cancelled = Arc::new(AtomicBool::new(false));
        let output_session = PlaybackSession::new();
        output_session
            .play(
                source.source(),
                FailingOutput(Arc::clone(&output_cancelled)),
            )
            .unwrap();
        assert_eq!(output_session.wait(), PlaybackState::Failed);
        assert!(matches!(
            output_session.last_error(),
            Some(PlaybackError::Output(_))
        ));
        assert!(output_cancelled.load(Ordering::Acquire));
        output_session.stop().unwrap();
    }

    #[test]
    fn separate_sessions_progress_independently_under_backpressure() {
        let source_a = TempWave::constant(24_000, 3000);
        let source_b = TempWave::constant(24_000, 6000);
        let observability = Observability::new(
            ObservabilityConfig::default()
                .with_filter("info")
                .expect("valid test filter"),
        )
        .expect("create observability context");
        let diagnostics = observability.diagnostics();
        let handle = observability.handle();
        let session_a = PlaybackSession::with_observability(handle.clone());
        let session_b = PlaybackSession::with_observability(handle);
        let (output_a, gate_a) = GateOutput::new();
        let (output_b, gate_b) = GateOutput::new();

        session_a.play(source_a.source(), output_a).unwrap();
        session_b.play(source_b.source(), output_b).unwrap();
        let mut playback_ids = std::collections::HashSet::new();
        let mut events_seen = Vec::new();
        while playback_ids.len() < 2 {
            let event = diagnostics
                .recv_timeout(TEST_TIMEOUT)
                .unwrap_or_else(|error| {
                    panic!("playback start diagnostics arrive; saw events {events_seen:?}: {error}")
                });
            events_seen.push(event.event.clone());
            if event.event == "core.playback.starting" {
                playback_ids.insert(event.playback_id.expect("playback ID field"));
            }
        }
        assert_eq!(
            playback_ids.len(),
            2,
            "sessions must have distinct correlation IDs"
        );
        assert!(gate_a.wait_for_calls(1));
        assert!(gate_b.wait_for_calls(1));
        assert!(gate_a.snapshot().accepted.is_empty());
        assert!(gate_b.snapshot().accepted.is_empty());

        gate_a.release(usize::MAX);
        assert_eq!(session_a.wait(), PlaybackState::Completed);
        assert!(gate_a.snapshot().finished);
        assert_eq!(session_b.state(), PlaybackState::Playing);
        assert_eq!(gate_b.snapshot().write_calls, 1);
        assert!(gate_b.snapshot().accepted.is_empty());
        assert_eq!(session_b.position().submitted, Duration::ZERO);

        gate_b.release(usize::MAX);
        assert_eq!(session_b.wait(), PlaybackState::Completed);
        session_a.stop().unwrap();
        session_b.stop().unwrap();
    }

    #[test]
    fn stop_and_seek_can_race_without_deadlock_or_stale_delivery() {
        let source = TempWave::constant(96_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let seeking_session = Arc::clone(&session);
        let seek_thread = thread::spawn(move || seeking_session.seek(Duration::from_secs(1)));
        assert!(wait_for_pending_seek(&session));
        session.stop().unwrap();
        assert!(matches!(
            seek_thread.join().unwrap(),
            Err(PlaybackError::Cancelled)
        ));

        let snapshot = gate.snapshot();
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(snapshot.cancelled);
        assert!(snapshot.accepted.is_empty());
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn stop_then_play_replaces_a_source_without_old_frames_after_stop() {
        let old_source = TempWave::constant(96_000, 1000);
        let new_source = TempWave::constant(4800, 12_000);
        let session = PlaybackSession::new();
        let (old_output, old_gate) = GateOutput::new();
        session.play(old_source.source(), old_output).unwrap();
        assert!(old_gate.wait_for_calls(1));
        old_gate.release(1);
        assert!(old_gate.wait_for_accepted(1));
        assert!(old_gate.wait_for_calls(2));

        session.stop().unwrap();
        let old_snapshot_at_stop = old_gate.snapshot();
        assert_eq!(old_snapshot_at_stop.accepted.len(), 1);
        assert!(old_snapshot_at_stop.cancelled);

        let output = NullOutput::new();
        let stats = output.stats_handle();
        session.play(new_source.source(), output).unwrap();
        assert_eq!(session.wait(), PlaybackState::Completed);
        assert_eq!(stats.snapshot().sample_frames_received, 4800);
        assert_eq!(
            old_gate.snapshot().accepted.len(),
            old_snapshot_at_stop.accepted.len()
        );
        session.stop().unwrap();
    }

    fn pause_at_first_blocked_frame(session: &Arc<PlaybackSession>, gate: &GateHandle) {
        assert!(gate.wait_for_calls(1));
        let pause_session = Arc::clone(session);
        let pause_thread = thread::spawn(move || pause_session.pause());
        assert!(wait_for_state(session, PlaybackState::Pausing));
        gate.release(1);
        assert!(pause_thread.join().unwrap().is_ok());
        assert_eq!(session.state(), PlaybackState::Paused);
    }

    fn wait_for_state(session: &PlaybackSession, expected: PlaybackState) -> bool {
        wait_for_condition(&session.shared, TEST_TIMEOUT, |data| data.state == expected)
    }

    fn wait_for_volume(session: &PlaybackSession, expected: f32) -> bool {
        wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
            data.volume == expected
        })
    }

    fn wait_for_processing_mode(
        session: &PlaybackSession,
        expected: PlaybackProcessingMode,
    ) -> bool {
        wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
            data.processing_mode == Some(expected)
        })
    }

    fn wait_for_pending_seek(session: &PlaybackSession) -> bool {
        wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
            data.pending_seek.is_some()
        })
    }

    fn wait_for_pending_filter_update(session: &PlaybackSession) -> bool {
        let data = lock(&session.shared.data);
        let (data, _) = session
            .shared
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                data.pending_filter_graph.is_none()
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.pending_filter_graph.is_some()
    }

    fn replace_graph_while_packet_sender_is_blocked(
        session: Arc<PlaybackSession>,
        gate: StepPacketHandle,
        expression: Option<&'static str>,
    ) {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker_session = Arc::clone(&session);
        let update_worker = thread::spawn(move || {
            let _ = result_tx.send(worker_session.set_filter_graph(expression));
        });
        assert!(wait_for_pending_filter_update(&session));

        for _ in 0..128 {
            match result_rx.try_recv() {
                Ok(result) => {
                    assert!(result.is_ok(), "filter replacement failed: {result:?}");
                    update_worker.join().expect("filter update worker joins");
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("filter update worker exited without returning a result");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }

            let next_call = gate.calls().saturating_add(1);
            gate.release(1);
            assert!(
                gate.wait_for_calls(next_call),
                "Opus sender did not reach the next packet while the update was pending"
            );
        }
        panic!("filter graph update did not reach a playback frame boundary");
    }

    fn wait_for_condition(
        shared: &SessionShared,
        timeout: Duration,
        condition: impl Fn(&SessionData) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut data = lock(&shared.data);
        while !condition(&data) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let result = shared.changed.wait_timeout(data, remaining);
            match result {
                Ok((next, wait_result)) => {
                    data = next;
                    if wait_result.timed_out() && !condition(&data) {
                        return false;
                    }
                }
                Err(poisoned) => data = poisoned.into_inner().0,
            }
        }
        true
    }

    fn pcm_wave(samples: &[i16]) -> Vec<u8> {
        let channels = 1_u16;
        let sample_rate = 48_000_u32;
        let bytes_per_sample = 2_u16;
        let block_align = channels * bytes_per_sample;
        let data_len = samples.len() * usize::from(bytes_per_sample);
        let mut bytes = Vec::with_capacity(44 + data_len);
        bytes.extend_from_slice(b"RIFF");
        push_u32(&mut bytes, (36 + data_len) as u32);
        bytes.extend_from_slice(b"WAVEfmt ");
        push_u32(&mut bytes, 16);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, channels);
        push_u32(&mut bytes, sample_rate);
        push_u32(&mut bytes, sample_rate * u32::from(block_align));
        push_u16(&mut bytes, block_align);
        push_u16(&mut bytes, 16);
        bytes.extend_from_slice(b"data");
        push_u32(&mut bytes, data_len as u32);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn samples_to_duration(samples: u64) -> Duration {
        let seconds = samples / 48_000;
        let remainder = samples % 48_000;
        let nanoseconds = (u128::from(remainder) * 1_000_000_000 / 48_000) as u32;
        Duration::new(seconds, nanoseconds)
    }
}
