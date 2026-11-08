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
    stage: Mutex<Option<Arc<PreparedStage>>>,
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

#[cfg(test)]
mod tests;

mod session;
mod staging;
pub use staging::{StageIdentity, StageSnapshot, StageState};
use staging::PreparedStage;

mod worker;
use worker::*;

mod control;
use control::*;

mod controls;
