use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use remedia_observability::{ObservabilityHandle, next_correlation_id};
use thiserror::Error;

use crate::crossfade::CrossfadeCurve;
use crate::decoder::{
    AudioDecoder, AudioStreamInfo, DecodedPcmFrame, DecoderOptions, DemuxedAudioPacket, PcmFrame,
    SourcePosition,
};
use crate::error::DecoderError;
use crate::filter::{FilterGraphError, PcmFilterGraph, PcmProcessor, ProcessError};
use crate::opus::EncodedOpusPacket;
use crate::output::{MediaOutput, OutputError};
use crate::source::{CancellationToken, InputSource, SourceReopener};
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
    /// A fallback seek was replaced by a newer seek, or the track changed (a crossfade began or
    /// the playback stopped being the one that was asked to seek) before its replacement source
    /// was ready. Playback continued on the source that was already playing.
    #[error("the seek was replaced before its replacement source was ready")]
    SeekSuperseded,
    /// The seek was cancelled before it took effect, by [`SeekCanceller::cancel`] or
    /// [`PlaybackSession::cancel_seek`]. Playback continued on the source that was playing.
    #[error("the seek was cancelled before it was applied")]
    SeekCancelled,
    #[error("seek position {target:?} is beyond media duration {duration:?}")]
    SeekOutOfRange {
        target: Duration,
        duration: Duration,
    },
    #[error("audio filtergraph error: {0}")]
    FilterGraph(#[source] Arc<FilterGraphError>),
    #[error("master volume must be a finite gain between 0.0 and 2.0, received {0}")]
    InvalidVolume(f32),
    /// Crossfade durations must satisfy the same bounds as `CrossfadeConfig::new`.
    #[error("invalid crossfade duration or preparation lead")]
    InvalidCrossfadeConfiguration,
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
    stage: Arc<Mutex<Option<Arc<PreparedStage>>>>,
    crossfade: Mutex<Option<CrossfadeConfig>>,
}

/// A seek that has been submitted to a session. See [`PlaybackSession::start_seek`].
///
/// Dropping it without waiting does not cancel the seek.
pub struct PendingSeek {
    response: mpsc::Receiver<Result<SeekOutcome, PlaybackError>>,
    shared: Arc<SessionShared>,
    stage: Arc<Mutex<Option<Arc<PreparedStage>>>>,
    ticket: Arc<SeekTicket>,
}

impl PendingSeek {
    /// A handle that cancels this seek from another thread, for example when the request that
    /// asked for it times out.
    pub fn canceller(&self) -> SeekCanceller {
        SeekCanceller {
            ticket: Arc::clone(&self.ticket),
            shared: Arc::clone(&self.shared),
        }
    }

    /// Block until the seek has been applied or rejected.
    ///
    /// Returns [`PlaybackError::SeekSuperseded`] when a newer seek replaced this one before its
    /// replacement source was ready, [`PlaybackError::SeekCancelled`] when it was cancelled, and
    /// [`PlaybackError::Cancelled`] when the playback was stopped. In every error case the source
    /// that was already playing keeps playing.
    pub fn wait(self) -> Result<(), PlaybackError> {
        self.wait_outcome().map(|_| ())
    }

    /// Like [`wait`](Self::wait), and report where the seek landed.
    pub fn wait_outcome(self) -> Result<SeekOutcome, PlaybackError> {
        let outcome = self.response.recv().unwrap_or_else(|_| {
            Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked))
        });
        if outcome.is_ok() {
            staging::clear_stage(&self.shared, &self.stage);
        }

        outcome
    }
}

/// A completion wait bound to the playback generation active when it was
/// created. Replacing a stopped source cannot make an older waiter follow the
/// new source.
pub struct PlaybackWaiter {
    shared: Arc<SessionShared>,
    generation: u64,
}

struct SessionShared {
    #[cfg(feature = "experimental-scheduler")]
    executor: std::sync::atomic::AtomicUsize,
    revision: std::sync::atomic::AtomicU64,
    #[cfg(feature = "experimental-scheduler")]
    scheduler: Option<Arc<PlaybackScheduler>>,
    #[cfg(feature = "experimental-scheduler")]
    scheduled: Mutex<Option<Arc<scheduler::Job>>>,
    data: Mutex<SessionData>,
    changed: Condvar,
    completion_waiters: completion::CompletionWaiters,
    observability: ObservabilityHandle,
    events: Mutex<crossfade_events::CrossfadeEventQueue>,
    crossfade_events: OnceLock<Arc<crossfade_events::CrossfadeEventSignal>>,
}

struct SessionData {
    state: PlaybackState,
    generation: u64,
    source_generation: u64,
    source_serial: u64,
    latest_stage: Option<StageIdentity>,
    /// The staged entry that was promoted into the active source, if any.
    active_stage: Option<StageIdentity>,
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
    /// The most recently submitted seek, for `PlaybackSession::cancel_seek`.
    seek_ticket: Option<Arc<SeekTicket>>,
    /// Fallback seeking is allowed for this session (see `configure_fallback_seeking`).
    fallback_seeking: bool,
    /// A fallback replacement finished preparing (or failed); the worker should look at it.
    fallback_signal: bool,
    filter_graph: Option<String>,
    pending_filter_graph: Option<FilterUpdate>,
    processor_initialized: bool,
    volume: f32,
    stopping: bool,
}

struct SeekRequest {
    position: Duration,
    response: SyncSender<Result<SeekOutcome, PlaybackError>>,
    ticket: Arc<SeekTicket>,
}

struct FilterUpdate {
    graph: Option<PcmFilterGraph>,
    response: SyncSender<Result<(), PlaybackError>>,
}

enum WorkerAction {
    #[cfg(feature = "experimental-scheduler")]
    DedicatedFallback,
    Wait(u64),
    ConfigureProcessing,
    PollFallback,
    Decode,
    Deliver(DecodedPcmFrame),
    Seek(SeekRequest),
    ReplaceFilter(FilterUpdate),
    Pause,
    Resume,
    Finish,
    Exit,
}

#[cfg(test)]
mod tests;

mod completion;
mod crossfade_events;
mod crossfade_worker;
mod fallback;
mod seek;
mod session;
mod staging;
use seek::SeekTicket;
pub use seek::{SeekCanceller, SeekOutcome};
use staging::PreparedStage;
pub use staging::{StageIdentity, StageSnapshot, StageState};

/// Session-scoped transition settings, validated before configuration.
/// Enabling crossfade also enables conservative smart scheduling and staged
/// leading-padding detection. Inconclusive analysis retains duration-based
/// timing. Network sources are never reopened for tail analysis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrossfadeConfig {
    /// Overlap duration in milliseconds, 1000 through 30000.
    pub duration_ms: u32,
    /// Lead before the overlap at which the next source is requested, in
    /// milliseconds, 5000 through 120000.
    pub prepare_before_ms: u32,
    /// How the two sources are weighted across the overlap.
    pub curve: CrossfadeCurve,
}

impl CrossfadeConfig {
    /// Validate a fade duration and preparation lead. The curve is
    /// [`CrossfadeCurve::EqualPower`]; choose another with [`Self::with_curve`].
    pub fn new(duration_ms: u32, prepare_before_ms: u32) -> Option<Self> {
        if !(1_000..=30_000).contains(&duration_ms)
            || !(5_000..=120_000).contains(&prepare_before_ms)
        {
            return None;
        }

        Some(Self {
            duration_ms,
            prepare_before_ms,
            curve: CrossfadeCurve::EqualPower,
        })
    }

    /// Use `curve` for the overlap.
    pub const fn with_curve(mut self, curve: CrossfadeCurve) -> Self {
        self.curve = curve;

        self
    }
}

/// Events only emitted for explicitly enabled staged playback.
#[derive(Clone, Debug)]
pub enum CrossfadeEvent {
    /// Older events were evicted after the bounded event queue filled.
    ///
    /// `count` is the number of discarded events since the previous drain. Consumers should
    /// discard inferred lifecycle history and reconcile against the session's current state,
    /// source generation, and staged-source snapshot.
    EventsDropped {
        /// Number of events lost from the queue.
        count: u64,
    },
    /// A seek changed the source timeline. `position_ms` is where the audio continues from;
    /// for a fallback seek that is later than `requested_position_ms` by the source time that
    /// elapsed while the replacement was prepared. `replaced_source` is set then, and the
    /// output timeline continued rather than restarting at the position.
    TimelineChanged {
        previous_generation: u64,
        generation: u64,
        position_ms: u64,
        requested_position_ms: u64,
        replaced_source: bool,
    },
    PrepareRequested {
        generation: u64,
        remaining_ms: u64,
    },
    StageReady {
        identity: StageIdentity,
    },
    StageFailed {
        identity: StageIdentity,
        reason: String,
    },
    StageInvalidated {
        identity: StageIdentity,
    },
    TransitionCancelled {
        outgoing_generation: u64,
        incoming_generation: u64,
        identity: StageIdentity,
    },
    TransitionStarted {
        outgoing_generation: u64,
        incoming_generation: u64,
        identity: StageIdentity,
        info: AudioStreamInfo,
    },
    TransitionCompleted {
        outgoing_generation: u64,
        incoming_generation: u64,
        identity: StageIdentity,
    },
}
impl SessionShared {
    fn publish(&self, event: CrossfadeEvent) {
        lock(&self.events).push(event);
    }

    /// Queue one crossfade event and notify subscribers. Callers must first release every
    /// playback-state and stage lock because a waker can synchronously poll from this method.
    pub(super) fn publish_crossfade_event(&self, event: CrossfadeEvent) {
        self.publish(event);
        self.notify_crossfade_events();
    }

    /// Publish after releasing the event queue and playback-state locks. Wakers may poll
    /// synchronously from `wake`, so callers must not invoke this while holding playback locks.
    /// The signal is initialized on first notification as well as first subscription so a
    /// terminal change racing with initial subscription cannot be lost.
    pub(super) fn notify_crossfade_events(&self) {
        self.crossfade_events
            .get_or_init(|| Arc::new(crossfade_events::CrossfadeEventSignal::default()))
            .notify();
    }
}

mod worker;
use worker::*;

mod control;
use control::*;

mod controls;

#[cfg(feature = "experimental-scheduler")]
mod scheduler;
#[cfg(feature = "experimental-scheduler")]
pub use scheduler::{PlaybackScheduler, SchedulerDiagnostics};

impl SessionShared {
    fn signal_change(&self) {
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.changed.notify_all();
        #[cfg(feature = "experimental-scheduler")]
        if let Some(job) = lock(&self.scheduled).as_ref() {
            job.wake();
        }
    }
}
