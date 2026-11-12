//! Zero-latency peak management for the mixed output.
//!
//! The overlap must keep the sample clock of the sources on either side of it,
//! so this limiter has no look-ahead delay. It reacts instantly to a sample
//! that would exceed full scale, then releases exponentially. The reduction is
//! stereo-linked, so one gain scales both channels and the image does not move.
//!
//! Samples that already fit are returned bit-for-bit unchanged, and the fast
//! path for them is a compare against the ceiling.

use crate::decoder::OUTPUT_SAMPLE_RATE_HZ;

/// Highest permitted output magnitude: digital full scale.
const CEILING: f32 = 1.0;

/// Release time constant in seconds. It is long enough not to modulate bass
/// cycles and short enough that a single stray peak is not audible as ducking.
const RELEASE_SECONDS: f64 = 0.02;

/// Reductions smaller than this (about 0.1 dB) are inaudible and are not
/// counted as limited frames.
const AUDIBLE_GAIN: f32 = 0.988;

/// The deepest reduction the limiter will apply. A far larger excess is left to
/// the caller's final safety clamp rather than ducking the programme for
/// hundreds of milliseconds.
const MIN_GAIN: f32 = 0.1;

/// Frames over which the limiter's influence is faded out at the end of a
/// window, so the audio rejoins the unprocessed source without a gain step.
pub(super) const TAIL_GUARD_FRAMES: usize = 480;

/// A bounded, stereo-linked, zero-latency peak limiter.
#[derive(Clone, Debug)]
pub(super) struct PeakLimiter {
    gain: f32,
    release: f32,
    limited_frames: u64,
    deepest_gain: f32,
}

impl PeakLimiter {
    pub(super) fn new() -> Self {
        Self {
            gain: 1.0,
            release: (1.0 - (-1.0 / (RELEASE_SECONDS * f64::from(OUTPUT_SAMPLE_RATE_HZ))).exp())
                as f32,
            limited_frames: 0,
            deepest_gain: 1.0,
        }
    }

    /// Frames processed while the limiter reduced gain by more than 0.1 dB.
    pub(super) const fn limited_frames(&self) -> u64 {
        self.limited_frames
    }

    /// The lowest gain the limiter has reached, in `MIN_GAIN..=1`.
    pub(super) const fn deepest_gain(&self) -> f32 {
        self.deepest_gain
    }

    /// The reduction currently being applied, in `MIN_GAIN..=1`.
    #[cfg(test)]
    pub(super) const fn gain(&self) -> f32 {
        self.gain
    }

    /// Limit one stereo frame of finite samples.
    ///
    /// `frames_to_end` is the number of frames remaining in the window after
    /// this one. Inside the final [`TAIL_GUARD_FRAMES`] the reduction is faded
    /// out linearly so that the last frame is passed through unmodified.
    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32, frames_to_end: usize) -> (f32, f32) {
        let peak = left.abs().max(right.abs());

        if self.gain >= 1.0 && peak <= CEILING {
            return (left, right);
        }

        let mut gain = self.gain + (1.0 - self.gain) * self.release;

        if 1.0 - gain < 1.0e-4 {
            gain = 1.0;
        }

        if peak > CEILING {
            gain = gain.min((CEILING / peak).max(MIN_GAIN));
        }

        self.gain = gain;

        if gain >= 1.0 {
            return (left, right);
        }

        self.deepest_gain = self.deepest_gain.min(gain);

        if gain < AUDIBLE_GAIN {
            self.limited_frames += 1;
        }

        let strength = (frames_to_end as f32 / TAIL_GUARD_FRAMES as f32).min(1.0);
        let applied = 1.0 - (1.0 - gain) * strength;

        (left * applied, right * applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAR: usize = usize::MAX;

    #[test]
    fn audio_within_range_is_untouched() {
        let mut limiter = PeakLimiter::new();

        for sample in [0.0, 0.5, -1.0, 1.0, 0.999_999] {
            assert_eq!(limiter.process(sample, -sample, FAR), (sample, -sample));
        }

        assert_eq!(limiter.limited_frames(), 0);
        assert_eq!(limiter.gain(), 1.0);
    }

    #[test]
    fn a_peak_is_brought_to_the_ceiling_without_clipping() {
        let mut limiter = PeakLimiter::new();
        let (left, right) = limiter.process(1.6, -0.8, FAR);

        assert!((left - 1.0).abs() < 1.0e-6);
        assert!((right + 0.5).abs() < 1.0e-6, "stereo ratio must hold");
        assert_eq!(limiter.limited_frames(), 1);
    }

    #[test]
    fn gain_releases_smoothly_back_to_unity() {
        let mut limiter = PeakLimiter::new();

        limiter.process(2.0, 2.0, FAR);

        let mut previous = limiter.gain();

        for _ in 0..48_000 {
            limiter.process(0.1, 0.1, FAR);

            let gain = limiter.gain();

            assert!(gain >= previous, "release must be monotonic");
            assert!(gain - previous < 0.001, "release must not jump");

            previous = gain;
        }

        assert_eq!(limiter.gain(), 1.0);
    }

    #[test]
    fn reduction_is_bounded_for_absurd_input() {
        let mut limiter = PeakLimiter::new();

        limiter.process(1.0e30, 0.0, FAR);

        assert!(limiter.gain() >= MIN_GAIN);
    }

    #[test]
    fn influence_fades_out_across_the_tail_guard() {
        let mut limiter = PeakLimiter::new();

        limiter.process(2.0, 2.0, FAR);

        assert_eq!(limiter.process(0.4, 0.4, 0), (0.4, 0.4));

        let mut limiter = PeakLimiter::new();

        limiter.process(2.0, 2.0, FAR);

        let (halfway, _) = limiter.process(0.4, 0.4, TAIL_GUARD_FRAMES / 2);

        assert!(halfway > 0.2 && halfway < 0.4);
    }
}
