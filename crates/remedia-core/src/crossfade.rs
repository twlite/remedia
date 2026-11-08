//! Bounded, sample-accurate crossfade math for normalized interleaved stereo PCM.
//!
//! The coordinator is responsible for opening sources, scheduling, and advancing
//! the output's continuous sample clock. This module never opens an input,
//! allocates a decoder, or writes to a Discord transport.

use std::collections::VecDeque;
use std::f32::consts::FRAC_PI_2;
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::decoder::OUTPUT_SAMPLE_RATE_HZ;

/// Playback settings for one overlapping transition. Durations are in
/// output samples, not encoded Opus packets or source stream timestamps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrossfadeWindow {
    samples: usize,
}

impl CrossfadeWindow {
    /// Create a fade duration in milliseconds; 1-30 seconds are supported.
    pub fn from_millis(duration_ms: u32) -> Option<Self> {
        if !(1_000..=30_000).contains(&duration_ms) {
            return None;
        }
        Some(Self {
            samples: usize::try_from(
                u64::from(duration_ms) * u64::from(OUTPUT_SAMPLE_RATE_HZ) / 1_000,
            )
            .ok()?,
        })
    }

    /// Number of normalized stereo sample frames in the window.
    pub const fn samples(self) -> usize {
        self.samples
    }
}

/// Equal-power fade over a bounded number of normalized stereo samples.
///
/// The first mixed sample contains the outgoing track only. The final sample
/// contains the incoming track only. Progress is continuous across calls,
/// even if input frames have different sizes. Both input slices contain
/// interleaved stereo f32 PCM with matching lengths.
#[derive(Clone, Debug)]
pub struct PcmCrossfade {
    window: CrossfadeWindow,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixError {
    /// The outgoing and incoming frames must have equal, even scalar counts.
    FrameMismatch,
    /// The fade has ended; the promoted source must own the output now.
    Complete,
}

impl PcmCrossfade {
    pub const fn new(window: CrossfadeWindow) -> Self {
        Self { window, cursor: 0 }
    }

    pub const fn consumed_samples(&self) -> usize {
        self.cursor
    }

    pub const fn remaining_samples(&self) -> usize {
        self.window.samples.saturating_sub(self.cursor)
    }

    pub const fn complete(&self) -> bool {
        self.cursor >= self.window.samples
    }

    /// Blend matching interleaved frames in place. Accepts partial frames and
    /// returns the number of stereo samples mixed. A caller must split frames
    /// at the fade boundary before invoking this method.
    pub fn mix(&mut self, outgoing: &mut [f32], incoming: &[f32]) -> Result<usize, MixError> {
        if outgoing.len() != incoming.len() || outgoing.len() % 2 != 0 {
            return Err(MixError::FrameMismatch);
        }
        let samples = outgoing.len() / 2;
        if samples > self.remaining_samples() {
            return Err(MixError::Complete);
        }
        // Never advance progress on an invalid frame.
        let denominator = self.window.samples.saturating_sub(1).max(1) as f32;
        for index in 0..samples {
            let phase = (self.cursor + index) as f32 / denominator;
            let angle = phase * FRAC_PI_2;
            let left_gain = angle.cos();
            let right_gain = angle.sin();
            let offset = index * 2;
            for channel in 0..2 {
                let old = outgoing[offset + channel];
                let new = incoming[offset + channel];
                let sample = if old.is_finite() && new.is_finite() {
                    old * left_gain + new * right_gain
                } else {
                    0.0
                };
                // Equal-power crossfades can exceed full scale with correlated
                // sources, so bound the output rather than forwarding invalid
                // samples into the Opus encoder.
                outgoing[offset + channel] =
                    if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 };
            }
        }
        self.cursor += samples;
        Ok(samples)
    }
}

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
        let capacity_scalars = usize::try_from(
            u64::from(capacity_ms) * u64::from(OUTPUT_SAMPLE_RATE_HZ) * 2 / 1_000,
        )
        .ok()?;
        Some(Self {
            capacity_scalars,
            state: Mutex::new(StagedPcmState::default()),
            writable: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, StagedPcmState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Submit interleaved stereo frames, blocking only the source decoder
    /// worker on backpressure. Calling cancel releases blocked producers.
    pub fn push(&self, frame: &[f32]) -> Result<(), StageBufferError> {
        if frame.len() % 2 != 0 {
            return Err(StageBufferError::InvalidFrame);
        }
        let mut remaining = frame;
        while !remaining.is_empty() {
            let mut state = self.lock();
            while state.pcm.len() == self.capacity_scalars
                && !state.cancelled
                && !state.finished
            {
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

    pub fn buffered_samples(&self) -> usize {
        self.lock().pcm.len() / 2
    }

    pub fn is_finished(&self) -> bool {
        let state = self.lock();
        state.finished && state.pcm.is_empty()
    }

    /// Mark a clean source EOF. Existing buffered frames remain readable.
    pub fn finish(&self) {
        self.lock().finished = true;
        self.writable.notify_all();
    }

    /// Invalidate the stage, discarding all buffered samples and waking its
    /// producer. An invalidated stage can never become ready again.
    pub fn cancel(&self) {
        let mut state = self.lock();
        state.cancelled = true;
        state.pcm.clear();
        drop(state);
        self.writable.notify_all();
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

    #[test]
    fn validates_fade_length_and_sample_count() {
        assert!(CrossfadeWindow::from_millis(0).is_none());
        assert!(CrossfadeWindow::from_millis(999).is_none());
        assert!(CrossfadeWindow::from_millis(30_001).is_none());
        assert_eq!(
            CrossfadeWindow::from_millis(5_000).unwrap().samples(),
            240_000
        );
    }

    #[test]
    fn fade_is_sample_accurate_and_continuous_across_arbitrary_chunks() {
        let window = CrossfadeWindow::from_millis(1_000).unwrap();
        let mut fade = PcmCrossfade::new(window);
        let left = vec![0.25; 2 * 18_000];
        let right = vec![0.5; 2 * 18_000];
        let mut first = left;
        assert_eq!(fade.mix(&mut first, &right), Ok(18_000));
        assert!((first[0] - 0.25).abs() < 0.0001);
        assert_eq!(fade.remaining_samples(), 30_000);

        let mut second = vec![0.25; 2 * 30_000];
        let incoming = vec![0.5; 2 * 30_000];
        assert_eq!(fade.mix(&mut second, &incoming), Ok(30_000));
        assert!(fade.complete());
        assert!((second[second.len() - 1] - 0.5).abs() < 0.0001);
    }

    #[test]
    fn rejects_invalid_chunks_without_advancing() {
        let mut fade = PcmCrossfade::new(CrossfadeWindow::from_millis(1_000).unwrap());
        assert_eq!(fade.mix(&mut [1.0], &[1.0]), Err(MixError::FrameMismatch));
        assert_eq!(fade.mix(&mut [1.0, 1.0], &[1.0]), Err(MixError::FrameMismatch));
        assert_eq!(fade.consumed_samples(), 0);
        let mut too_long = vec![0.0; 2 * 48_001];
        assert_eq!(
            fade.mix(&mut too_long, &vec![0.0; 2 * 48_001]),
            Err(MixError::Complete)
        );
        assert_eq!(fade.consumed_samples(), 0);
    }

    #[test]
    fn mixing_correlated_sources_stays_in_range() {
        let mut fade = PcmCrossfade::new(CrossfadeWindow::from_millis(1_000).unwrap());
        let mut outgoing = vec![1.0; 2 * 48_000];
        let incoming = vec![1.0; 2 * 48_000];
        fade.mix(&mut outgoing, &incoming).unwrap();
        assert!(outgoing.iter().all(|sample| sample.is_finite() && sample.abs() <= 1.0));
    }
}
