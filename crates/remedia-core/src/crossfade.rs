//! Bounded, sample-accurate crossfade math for normalized interleaved stereo PCM.
//!
//! The coordinator is responsible for opening sources, scheduling, and advancing
//! the output's continuous sample clock. This module never opens an input,
//! allocates a decoder, or writes to a Discord transport.

use std::f32::consts::FRAC_PI_2;

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

#[cfg(test)]
mod tests {
    use super::*;

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
