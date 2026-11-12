//! Bounded staged PCM buffering for the incoming source of a transition.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::decoder::OUTPUT_SAMPLE_RATE_HZ;

/// A bounded, cancellation-aware PCM staging buffer.
///
/// A source decoder can fill this buffer independently while the current track
/// continues to output audio. Staging never sends packets to the voice sink.
/// When full, producers block until the mixer consumes audio or the stage is
/// invalidated. A consumer never blocks the real-time output thread.
pub struct StagedPcmBuffer {
    capacity_scalars: usize,
    state: Mutex<StagedPcmState>,
    writable: Condvar,
    readable: Condvar,
}

#[derive(Default)]
struct StagedPcmState {
    pcm: VecDeque<f32>,
    finished: bool,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageBufferError {
    InvalidFrame,
    Cancelled,
    Closed,
}

impl StagedPcmBuffer {
    /// Construct a bounded buffer of normalized 48 kHz stereo PCM.
    /// The recommended capacity is 2 seconds, approximately 768 KiB.
    pub fn with_capacity_ms(capacity_ms: u32) -> Option<Self> {
        if !(100..=10_000).contains(&capacity_ms) {
            return None;
        }
        let capacity_scalars =
            usize::try_from(u64::from(capacity_ms) * u64::from(OUTPUT_SAMPLE_RATE_HZ) * 2 / 1_000)
                .ok()?;
        Some(Self {
            capacity_scalars,
            state: Mutex::new(StagedPcmState::default()),
            writable: Condvar::new(),
            readable: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, StagedPcmState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Submit interleaved stereo frames, blocking only the source decoder
    /// worker on backpressure. Calling cancel releases blocked producers.
    pub fn push(&self, frame: &[f32]) -> Result<(), StageBufferError> {
        if !frame.len().is_multiple_of(2) {
            return Err(StageBufferError::InvalidFrame);
        }
        let mut remaining = frame;
        while !remaining.is_empty() {
            let mut state = self.lock();
            while state.pcm.len() == self.capacity_scalars && !state.cancelled && !state.finished {
                state = self
                    .writable
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.cancelled {
                return Err(StageBufferError::Cancelled);
            }
            if state.finished {
                return Err(StageBufferError::Closed);
            }
            let space = self.capacity_scalars - state.pcm.len();
            // The capacity is even, and consumers only read stereo pairs.
            let count = remaining.len().min(space);
            state.pcm.extend(remaining[..count].iter().copied());
            remaining = &remaining[count..];
            self.readable.notify_all();
        }
        Ok(())
    }

    /// Return exactly `samples_per_channel` frames without blocking, or
    /// None if not enough audio is ready. This is safe on the output thread.
    /// A request larger than the configured buffer always returns None.
    pub fn try_pop_exact(&self, samples_per_channel: usize) -> Option<Vec<f32>> {
        let scalars = samples_per_channel.checked_mul(2)?;
        if scalars > self.capacity_scalars {
            return None;
        }
        let mut state = self.lock();
        if state.cancelled || state.pcm.len() < scalars {
            return None;
        }
        let popped = state.pcm.drain(..scalars).collect();
        drop(state);
        self.writable.notify_all();
        Some(popped)
    }

    /// Wait for the requested number of stereo samples, returning a shorter
    /// final frame at clean EOF. Cancellation is checked during every wait.
    /// A failed or replaced stage wakes the consumer with None.
    pub fn read_for_playback(
        &self,
        samples_per_channel: usize,
        cancellation: &crate::CancellationToken,
    ) -> Option<Vec<f32>> {
        let scalars = samples_per_channel.checked_mul(2)?;
        if scalars == 0 || scalars > self.capacity_scalars {
            return None;
        }
        let mut state = self.lock();
        while state.pcm.len() < scalars
            && !state.finished
            && !state.cancelled
            && !cancellation.is_cancelled()
        {
            let (next, _) = self
                .readable
                .wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
        if state.cancelled || cancellation.is_cancelled() || state.pcm.is_empty() {
            return None;
        }
        let count = scalars.min(state.pcm.len());
        let count = count - count % 2;
        let result = state.pcm.drain(..count).collect();
        drop(state);
        self.writable.notify_all();
        Some(result)
    }

    pub fn buffered_samples(&self) -> usize {
        self.lock().pcm.len() / 2
    }

    pub fn is_finished(&self) -> bool {
        let state = self.lock();
        state.finished
    }

    /// Mark a clean source EOF. Existing buffered frames remain readable.
    pub fn finish(&self) {
        self.lock().finished = true;
        self.writable.notify_all();
        self.readable.notify_all();
    }

    /// Invalidate the stage, discarding all buffered samples and waking its
    /// producer. An invalidated stage can never become ready again.
    pub fn cancel(&self) {
        let mut state = self.lock();
        state.cancelled = true;
        state.pcm.clear();
        drop(state);
        self.writable.notify_all();
        self.readable.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_buffer_is_bounded_and_nonblocking_for_the_mixer() {
        let buffer = StagedPcmBuffer::with_capacity_ms(100).unwrap();
        let mut frames = vec![0.0; 2 * 4_800];
        frames[0] = 0.25;
        buffer.push(&frames).unwrap();
        assert_eq!(buffer.buffered_samples(), 4_800);
        assert!(buffer.try_pop_exact(4_801).is_none());
        assert!(buffer.try_pop_exact(4_800).is_some());
        assert_eq!(buffer.buffered_samples(), 0);
        buffer.finish();
        assert!(buffer.is_finished());
        assert_eq!(buffer.push(&frames), Err(StageBufferError::Closed));
    }

    #[test]
    fn cancelling_stage_releases_blocked_decoder() {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;
        let buffer = Arc::new(StagedPcmBuffer::with_capacity_ms(100).unwrap());
        buffer.push(&vec![0.0; 9_600]).unwrap();
        let pending = Arc::clone(&buffer);
        let worker = thread::spawn(move || pending.push(&[0.5, 0.5]));
        thread::sleep(Duration::from_millis(5));
        buffer.cancel();
        assert_eq!(worker.join().unwrap(), Err(StageBufferError::Cancelled));
        assert_eq!(buffer.buffered_samples(), 0);
    }
}
