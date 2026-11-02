use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::decoder::{OUTPUT_SAMPLE_RATE_HZ, PcmFrame};
use crate::opus::EncodedOpusPacket;
use crate::source::CancellationToken;

/// An error returned by a media output.
#[derive(Clone, Debug, Error)]
pub enum OutputError {
    #[error("media output failed: {message}")]
    Failure {
        message: String,
        #[source]
        source: Option<Arc<dyn Error + Send + Sync>>,
    },
    #[error("media output operation was cancelled")]
    Cancelled,
}

impl OutputError {
    /// Create an output error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self::Failure {
            message: message.into(),
            source: None,
        }
    }

    /// Preserve an underlying error as the output failure's source.
    pub fn with_source(error: impl Error + Send + Sync + 'static) -> Self {
        let message = error.to_string();
        Self::Failure {
            message,
            source: Some(Arc::new(error)),
        }
    }
}

/// A consumer for normalized PCM frames.
///
/// One output value is created for each playback. `write_frame` is synchronous:
/// returning successfully means the output accepted the frame, so a slow output
/// naturally applies backpressure to decoding. Implementations may buffer
/// internally, but must bound that buffering. Long-running operations must
/// observe `cancellation`; `cancel` is called concurrently to unblock them.
pub trait MediaOutput: Send + Sync + 'static {
    /// Accept one owned frame. The frame is interleaved 48 kHz stereo `f32`.
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;

    /// Return an optional encoded-Opus capability. PCM-only outputs keep their
    /// existing contract; outputs such as [`crate::OpusEncoderOutput`] can opt
    /// in so a compatible source can bypass decode and encode work.
    fn encoded_opus_output(&self) -> Option<&dyn EncodedOpusOutput> {
        None
    }

    /// Discard buffered output after a successful seek and reset its timeline.
    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;

    /// Discard pre-seek data and reset the output at an exact normalized PCM
    /// sample. The default preserves compatibility with outputs that only
    /// implement the duration-based [`Self::flush`] operation.
    fn flush_samples(
        &self,
        sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.flush(duration_from_sample_position(sample_position), cancellation)
    }

    /// Notify the output that a replacement filtergraph will supply the next
    /// PCM samples. Implementations should preserve their timeline and accepted
    /// audio; unlike [`Self::flush_samples`], this is not a seek or transport
    /// generation change.
    fn filter_transition(
        &self,
        _next_sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            Err(OutputError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Suspend downstream output after the current frame boundary. Outputs
    /// which do not schedule or transmit in real time can keep the default.
    fn pause(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    /// Resume downstream output after [`Self::pause`].
    fn resume(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    /// Finish successfully after decoder end-of-stream.
    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError>;

    /// Cancel pending output and unblock an in-progress `write_frame`, `flush`,
    /// or `finish` call. This may run concurrently with those methods.
    fn cancel(&self);

    /// Return an audible/consumed position when the output can report one.
    /// Outputs that only accept or buffer frames should return `None`. This is
    /// called from session control threads and should return without blocking.
    fn played_position(&self) -> Option<Duration> {
        None
    }
}

/// Optional capability for outputs that can accept both normalized PCM and
/// already-encoded Opus packets through one playback-scoped sink.
///
/// This is separate from [`MediaOutput`] so null and custom PCM consumers do
/// not need to implement compressed-packet handling.
pub trait EncodedOpusOutput: Send + Sync {
    /// Whether the packet's duration and payload meet this output's
    /// transport-independent acceptance policy.
    fn supports_encoded_packet(&self, packet: &EncodedOpusPacket) -> bool;

    /// Submit an owned source Opus packet to the same sink used by PCM
    /// encoding. `start_sample` is on the continuous normalized output
    /// timeline, not the source container timestamp.
    fn write_encoded_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;

    /// Close a partial PCM encoder frame at a safe packet boundary before
    /// switching back to encoded packets. Implementations must preserve sink
    /// ordering and must not seek, flush, or replace the underlying transport.
    fn prepare_passthrough(
        &self,
        next_sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;
}

/// Statistics recorded by [`NullOutput`]. `samples_received` counts scalar
/// interleaved `f32` values, while `sample_frames_received` counts time-aligned
/// samples per channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NullOutputStats {
    pub frames_received: u64,
    pub samples_received: u64,
    pub sample_frames_received: u64,
    pub duration: Duration,
    pub finished_successfully: bool,
}

/// A handle for reading statistics while or after a session uses a null output.
#[derive(Clone, Debug)]
pub struct NullOutputStatsHandle(Arc<Mutex<NullOutputState>>);

impl NullOutputStatsHandle {
    /// Read a consistent statistics snapshot.
    pub fn snapshot(&self) -> NullOutputStats {
        lock(&self.0).stats
    }
}

/// A no-allocation consumer useful for tests, examples, and decode benchmarks.
///
/// It drops each owned PCM buffer after recording its counts and timeline.
#[derive(Debug, Default)]
pub struct NullOutput {
    state: Arc<Mutex<NullOutputState>>,
}

#[derive(Debug, Default)]
struct NullOutputState {
    stats: NullOutputStats,
    position: Duration,
    cancelled: bool,
}

impl NullOutput {
    /// Create a null output with empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a lightweight handle that can inspect this output after it is moved
    /// into a playback session.
    pub fn stats_handle(&self) -> NullOutputStatsHandle {
        NullOutputStatsHandle(Arc::clone(&self.state))
    }
}

impl MediaOutput for NullOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }

        let mut state = lock(&self.state);
        if state.cancelled {
            return Err(OutputError::Cancelled);
        }
        state.stats.frames_received = state.stats.frames_received.saturating_add(1);
        state.stats.samples_received = state
            .stats
            .samples_received
            .saturating_add(frame.samples.len() as u64);
        state.stats.sample_frames_received = state
            .stats
            .sample_frames_received
            .saturating_add(frame.samples_per_channel as u64);
        let end_sample = frame
            .start_sample
            .saturating_add(frame.samples_per_channel as u64);
        state.position = duration_from_sample_position(end_sample);
        state.stats.duration = duration_from_sample_count(state.stats.sample_frames_received);
        state.stats.finished_successfully = false;
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

        let mut state = lock(&self.state);
        if state.cancelled {
            return Err(OutputError::Cancelled);
        }
        state.position = position;
        state.stats.finished_successfully = false;
        Ok(())
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }

        let mut state = lock(&self.state);
        if state.cancelled {
            return Err(OutputError::Cancelled);
        }
        state.stats.finished_successfully = true;
        Ok(())
    }

    fn cancel(&self) {
        let mut state = lock(&self.state);
        state.cancelled = true;
        state.stats.finished_successfully = false;
    }

    fn played_position(&self) -> Option<Duration> {
        Some(lock(&self.state).position)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn duration_from_sample_position(sample_position: u64) -> Duration {
    duration_from_sample_count(sample_position)
}

fn duration_from_sample_count(sample_count: u64) -> Duration {
    let rate = u64::from(OUTPUT_SAMPLE_RATE_HZ);
    let seconds = sample_count / rate;
    let remainder = sample_count % rate;
    let nanoseconds = (u128::from(remainder) * 1_000_000_000 / u128::from(rate)) as u32;
    Duration::new(seconds, nanoseconds)
}
