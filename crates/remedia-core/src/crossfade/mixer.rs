//! Sample-accurate mixing of the outgoing and incoming PCM of a transition.

use super::CrossfadeWindow;
use super::analysis::{HOP_FRAMES, HopSums};
use super::curves::{AdaptiveGain, CrossfadeCurve, equal_power_gains};
use super::limiter::PeakLimiter;

/// Errors from [`PcmCrossfade::mix`]. A failed call never advances the fade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixError {
    /// The outgoing and incoming frames must have equal, even scalar counts.
    FrameMismatch,
    /// The fade has ended; the promoted source must own the output now.
    Complete,
}

/// Counters describing what the mixer has done to the audio so far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixStats {
    /// The deepest loudness-aware common gain scheduled so far, in decibels.
    /// Always within `-6.0..=0.0`, and exactly `0.0` for
    /// [`CrossfadeCurve::EqualPower`] or when the sources were silent.
    pub deepest_adaptive_gain_db: f32,
    /// Stereo frames the peak limiter attenuated by more than 0.1 dB because
    /// the mix would otherwise have exceeded full scale.
    pub limited_frames: u64,
    /// The deepest reduction the peak limiter has applied, in decibels. Zero
    /// when no peak has needed limiting, and never above `20.0`.
    pub peak_reduction_db: f32,
}

/// A crossfade over a bounded number of normalized stereo samples.
///
/// The first mixed sample contains the outgoing track only. The final sample
/// contains the incoming track only. Progress is continuous across calls,
/// even if input frames have different sizes. Both input slices contain
/// interleaved stereo f32 PCM with matching lengths.
///
/// The curve decides the gains: see [`CrossfadeCurve`]. For every curve, peaks
/// are managed by a bounded limiter and a final clamp to `-1.0..=1.0` is kept
/// only as a safety net. The mixer performs no allocation.
#[derive(Clone, Debug)]
pub struct PcmCrossfade {
    window: CrossfadeWindow,
    cursor: usize,
    curve: CrossfadeCurve,
    adaptive: Option<AdaptiveGain>,
    limiter: PeakLimiter,
}

impl PcmCrossfade {
    /// Create an equal-power fade over `window`.
    pub fn new(window: CrossfadeWindow) -> Self {
        Self::with_curve(window, CrossfadeCurve::EqualPower)
    }

    /// Create a fade over `window` that uses `curve`.
    pub fn with_curve(window: CrossfadeWindow, curve: CrossfadeCurve) -> Self {
        Self {
            window,
            cursor: 0,
            curve,
            adaptive: (curve == CrossfadeCurve::Adaptive)
                .then(|| AdaptiveGain::new(window.samples())),
            limiter: PeakLimiter::new(),
        }
    }

    /// The curve this fade was created with.
    pub const fn curve(&self) -> CrossfadeCurve {
        self.curve
    }

    pub const fn consumed_samples(&self) -> usize {
        self.cursor
    }

    pub const fn remaining_samples(&self) -> usize {
        self.window.samples().saturating_sub(self.cursor)
    }

    pub const fn complete(&self) -> bool {
        self.cursor >= self.window.samples()
    }

    /// What the mixer has applied to the audio so far.
    pub fn stats(&self) -> MixStats {
        MixStats {
            deepest_adaptive_gain_db: self
                .adaptive
                .as_ref()
                .map_or(0.0, AdaptiveGain::deepest_gain_db),
            limited_frames: self.limiter.limited_frames(),
            peak_reduction_db: match self.limiter.deepest_gain() {
                gain if gain < 1.0 => -20.0 * gain.log10(),
                _ => 0.0,
            },
        }
    }

    /// Blend matching interleaved frames in place. Accepts partial frames and
    /// returns the number of stereo samples mixed. A caller must split frames
    /// at the fade boundary before invoking this method.
    pub fn mix(&mut self, outgoing: &mut [f32], incoming: &[f32]) -> Result<usize, MixError> {
        if outgoing.len() != incoming.len() || !outgoing.len().is_multiple_of(2) {
            return Err(MixError::FrameMismatch);
        }

        let samples = outgoing.len() / 2;

        // Never advance progress on an invalid frame.
        if samples > self.remaining_samples() {
            return Err(MixError::Complete);
        }

        let window_frames = self.window.samples();
        let limiter = &mut self.limiter;

        match self.adaptive.as_mut() {
            None => {
                let span = Span {
                    first: self.cursor,
                    window_frames,
                };

                mix_span::<false>(span, limiter, None, outgoing, incoming);
            }
            Some(adaptive) => {
                let mut done = 0;

                // Analysis hops are aligned to the window, not to the block, so
                // the result is identical for any chunking of the audio.
                while done < samples {
                    let first = self.cursor + done;
                    let into_hop = first % HOP_FRAMES;

                    if into_hop == 0 {
                        adaptive.begin_hop(first);
                    }

                    let count = (samples - done).min(HOP_FRAMES - into_hop);
                    let range = done * 2..(done + count) * 2;
                    let span = Span {
                        first,
                        window_frames,
                    };

                    mix_span::<true>(
                        span,
                        limiter,
                        Some(adaptive),
                        &mut outgoing[range.clone()],
                        &incoming[range],
                    );

                    done += count;
                }
            }
        }

        self.cursor += samples;

        Ok(samples)
    }
}

/// Where a run of frames sits in the fade window.
#[derive(Clone, Copy)]
struct Span {
    /// Window position of the run's first frame.
    first: usize,
    window_frames: usize,
}

/// Mix one run that does not cross an analysis hop boundary.
#[inline]
fn mix_span<const ADAPTIVE: bool>(
    span: Span,
    limiter: &mut PeakLimiter,
    adaptive: Option<&mut AdaptiveGain>,
    outgoing: &mut [f32],
    incoming: &[f32],
) {
    let denominator = span.window_frames.saturating_sub(1).max(1) as f32;
    let last_frame = span.window_frames.saturating_sub(1);
    let mut adaptive = adaptive.filter(|_| ADAPTIVE);

    // Continue the running sums exactly where the previous run stopped, so that
    // rounding does not depend on how the audio was chunked.
    let (mut sums, start_gain, slope, hop_start) = match adaptive.as_deref_mut() {
        Some(adaptive) => {
            let (start_gain, slope) = adaptive.ramp();

            (
                adaptive.analyzer_mut().sums,
                start_gain,
                slope,
                adaptive.hop_start(),
            )
        }
        None => (HopSums::default(), 1.0, 0.0, 0),
    };

    for (index, (old_pair, new_pair)) in outgoing
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(incoming.as_chunks::<2>().0)
        .enumerate()
    {
        let position = span.first + index;
        let (outgoing_gain, incoming_gain) = equal_power_gains(position as f32 / denominator);

        // A non-finite sample from either source silences that channel
        // rather than reaching the Opus encoder.
        let (old_left, new_left) = sanitize(old_pair[0], new_pair[0]);
        let (old_right, new_right) = sanitize(old_pair[1], new_pair[1]);
        let mut left = old_left * outgoing_gain + new_left * incoming_gain;
        let mut right = old_right * outgoing_gain + new_right * incoming_gain;

        if ADAPTIVE {
            sums.outgoing += old_left * old_left + old_right * old_right;
            sums.incoming += new_left * new_left + new_right * new_right;
            sums.mix += left * left + right * right;

            let gain = start_gain + slope * (position - hop_start) as f32;

            left *= gain;
            right *= gain;
        }

        let (left, right) = limiter.process(left, right, last_frame.saturating_sub(position));

        // Final safety net only; the limiter normally keeps peaks in range.
        old_pair[0] = left.clamp(-1.0, 1.0);
        old_pair[1] = right.clamp(-1.0, 1.0);
    }

    if let Some(adaptive) = adaptive {
        adaptive.analyzer_mut().sums = sums;
    }
}

/// Return both samples, or zeros if either is not finite.
#[inline]
fn sanitize(old: f32, new: f32) -> (f32, f32) {
    if old.is_finite() && new.is_finite() {
        (old, new)
    } else {
        (0.0, 0.0)
    }
}
