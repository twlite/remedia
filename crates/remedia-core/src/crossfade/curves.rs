//! Crossfade gain curves.
//!
//! [`CrossfadeCurve::EqualPower`] applies a fixed trigonometric fade.
//! [`CrossfadeCurve::Adaptive`] starts from the same fade and adds one bounded,
//! smoothed gain that is shared by both sources and both channels, so stereo
//! balance and the fade shape are untouched. This module owns the control law
//! and its smoothing; [`super::analysis`] supplies the level estimates.

use std::f32::consts::FRAC_PI_2;

use super::analysis::{HOP_FRAMES, OverlapAnalyzer, OverlapLevels};

/// The deepest attenuation the adaptive curve may ever apply, in decibels.
pub(super) const MAX_CUT_DB: f64 = 6.0;

/// Fraction of the window over which the adjustment is faded in from, and out
/// to, unity gain.
const EDGE_RAMP: f64 = 0.2;

/// Number of hops at each end of the window that always run at exactly unity
/// gain, so the audio joins the unprocessed neighbouring source seamlessly
/// whatever the window length.
const EDGE_GUARD_HOPS: f64 = 2.0;

/// Time constant of the gain smoother, in seconds.
const GAIN_SMOOTHING_SECONDS: f64 = 0.2;

/// The largest gain movement allowed in a single hop, in decibels. This is
/// 10 dB per second and keeps the adjustment from audibly pumping.
const MAX_SLEW_DB_PER_HOP: f64 = 0.1;

/// Level below which a source counts as silent, in decibels relative to full
/// scale (mean-square power).
const SILENCE_DB: f64 = -70.0;

/// Level above which a source is trusted to drive the adjustment fully.
const FULL_CONFIDENCE_DB: f64 = -55.0;

/// Power floor used before converting to decibels.
const POWER_FLOOR: f64 = 1.0e-10;

/// How two overlapping sources are weighted across a transition.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum CrossfadeCurve {
    /// Fixed equal-power gains with no level-dependent processing. This is the
    /// default.
    #[default]
    EqualPower,
    /// Equal-power gains plus a bounded, smoothed, loudness-aware common gain.
    /// The adjustment only ever attenuates, by at most 6 dB.
    Adaptive,
}

impl CrossfadeCurve {
    /// Every supported curve, in documentation order.
    pub const ALL: [Self; 2] = [Self::EqualPower, Self::Adaptive];

    /// The stable name used by configuration, the RPC protocol, and the Node
    /// bindings.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EqualPower => "equalPower",
            Self::Adaptive => "adaptive",
        }
    }

    /// Parse a stable curve name. Names are case sensitive.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|curve| curve.as_str() == name)
    }
}

/// Equal-power gains `(outgoing, incoming)` at a fade phase of `0.0..=1.0`.
///
/// Phase 0 returns exactly `(1, 0)` and phase 1 exactly `(0, 1)`, so a fade
/// joins the surrounding unprocessed audio without a residual offset.
#[inline]
pub(super) fn equal_power_gains(phase: f32) -> (f32, f32) {
    if phase >= 1.0 {
        return (0.0, 1.0);
    }

    let (incoming, outgoing) = (phase * FRAC_PI_2).sin_cos();

    (outgoing, incoming)
}

/// Hermite smoothstep of `x` clamped to `0..=1`.
pub(super) fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);

    x * x * (3.0 - 2.0 * x)
}

/// Convert mean-square power to decibels relative to full scale.
pub(super) fn power_to_db(power: f64) -> f64 {
    10.0 * power.max(POWER_FLOOR).log10()
}

/// Convert decibels to a linear amplitude gain. Zero maps to exactly 1.
pub(super) fn db_to_gain(db: f64) -> f32 {
    if db == 0.0 {
        1.0
    } else {
        10f64.powf(db / 20.0) as f32
    }
}

/// How far the adjustment is allowed to act at a point in the fade.
///
/// The weight is exactly zero for [`EDGE_GUARD_HOPS`] hops at each end and
/// reaches one after a smooth [`EDGE_RAMP`], which makes the combined gain
/// continuous with the unprocessed audio on both sides of the window.
pub(super) fn edge_weight(position: f64, window_frames: usize) -> f64 {
    let last = window_frames.saturating_sub(1).max(1) as f64;
    let position = position.clamp(0.0, last);
    let guard = EDGE_GUARD_HOPS * HOP_FRAMES as f64;
    let nearest_edge = position.min(last - position);

    smoothstep((nearest_edge - guard) / (EDGE_RAMP * last))
}

/// Confidence in `0..=1` that both sources carry programme material.
///
/// Silent or near-silent sources give no usable level, so the adjustment fades
/// out instead of reacting to noise floors.
fn confidence(outgoing_db: f64, incoming_db: f64) -> f64 {
    let quieter = outgoing_db.min(incoming_db);

    smoothstep((quieter - SILENCE_DB) / (FULL_CONFIDENCE_DB - SILENCE_DB))
}

/// The raw, unsmoothed common gain in decibels, within `-MAX_CUT_DB..=0`.
///
/// The target output level moves from the outgoing level to the incoming level
/// in the decibel domain along a smoothstep. The measured level of the actual
/// equal-power mix (which includes any correlation between the sources) is
/// subtracted from it. A positive result would be a boost, so it is clamped to
/// zero.
pub(super) fn target_gain_db(levels: OverlapLevels, progress: f64) -> f64 {
    let blend = smoothstep(progress);
    let desired = (1.0 - blend) * levels.outgoing_db + blend * levels.incoming_db;
    let needed = (desired - levels.mix_db).clamp(-MAX_CUT_DB, 0.0);

    needed * confidence(levels.outgoing_db, levels.incoming_db)
}

/// Loudness-aware gain state for one transition.
///
/// The gain is piecewise linear in amplitude between knots placed every
/// [`HOP_FRAMES`] frames of the window, so it never depends on how the caller
/// chunks the PCM. A knot is computed only from audio that has already been
/// mixed; there is no feedback from the applied gain into the measurement.
#[derive(Clone, Debug)]
pub(super) struct AdaptiveGain {
    analyzer: OverlapAnalyzer,
    window_frames: usize,
    smoothed_db: f64,
    smoothing: f64,
    knot_db: f64,
    deepest_db: f64,
    start_gain: f32,
    end_gain: f32,
    hop_start: usize,
}

impl AdaptiveGain {
    pub(super) fn new(window_frames: usize) -> Self {
        Self {
            analyzer: OverlapAnalyzer::new(),
            window_frames,
            smoothed_db: 0.0,
            smoothing: 1.0 - (-(HOP_FRAMES as f64) / (GAIN_SMOOTHING_SECONDS * 48_000.0)).exp(),
            knot_db: 0.0,
            deepest_db: 0.0,
            start_gain: 1.0,
            end_gain: 1.0,
            hop_start: 0,
        }
    }

    /// The most attenuation scheduled so far, in decibels. Never positive and
    /// never below `-MAX_CUT_DB`.
    pub(super) fn deepest_gain_db(&self) -> f32 {
        // Adding zero turns a negative zero into a positive one.
        self.deepest_db as f32 + 0.0
    }

    /// Start the hop that begins at window frame `position`, a multiple of
    /// [`HOP_FRAMES`]. The hop that just finished is folded into the level
    /// estimates first, and the next knot is scheduled from them.
    pub(super) fn begin_hop(&mut self, position: usize) {
        debug_assert_eq!(position % HOP_FRAMES, 0);

        if position > 0 {
            self.analyzer.close_hop();
        }

        let next_knot = position + HOP_FRAMES;

        if let Some(levels) = self.analyzer.levels() {
            let last = self.window_frames.saturating_sub(1).max(1) as f64;
            let progress = (next_knot as f64 / last).min(1.0);
            let target = target_gain_db(levels, progress);
            let step = ((target - self.smoothed_db) * self.smoothing)
                .clamp(-MAX_SLEW_DB_PER_HOP, MAX_SLEW_DB_PER_HOP);

            self.smoothed_db = (self.smoothed_db + step).clamp(-MAX_CUT_DB, 0.0);
        }

        self.knot_db = self.smoothed_db * edge_weight(next_knot as f64, self.window_frames);
        self.deepest_db = self.deepest_db.min(self.knot_db);
        self.start_gain = self.end_gain;
        self.end_gain = db_to_gain(self.knot_db);
        self.hop_start = position;
    }

    /// The amplitude gain slope per frame and the gain at the start of the
    /// current hop, as `(start, slope)`.
    #[inline]
    pub(super) fn ramp(&self) -> (f32, f32) {
        (
            self.start_gain,
            (self.end_gain - self.start_gain) / HOP_FRAMES as f32,
        )
    }

    /// Window frame at which the current hop's ramp began.
    #[inline]
    pub(super) fn hop_start(&self) -> usize {
        self.hop_start
    }

    /// Mutable access to the per-hop energy accumulators.
    #[inline]
    pub(super) fn analyzer_mut(&mut self) -> &mut OverlapAnalyzer {
        &mut self.analyzer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(outgoing_db: f64, incoming_db: f64, mix_db: f64) -> OverlapLevels {
        OverlapLevels {
            outgoing_db,
            incoming_db,
            mix_db,
        }
    }

    #[test]
    fn curve_names_round_trip_and_default_to_equal_power() {
        assert_eq!(CrossfadeCurve::default(), CrossfadeCurve::EqualPower);

        for curve in CrossfadeCurve::ALL {
            assert_eq!(CrossfadeCurve::from_name(curve.as_str()), Some(curve));
        }

        assert_eq!(CrossfadeCurve::from_name("linear"), None);
        assert_eq!(CrossfadeCurve::from_name("Adaptive"), None);
    }

    #[test]
    fn equal_power_gains_hit_both_endpoints_exactly() {
        assert_eq!(equal_power_gains(0.0), (1.0, 0.0));
        assert_eq!(equal_power_gains(1.0), (0.0, 1.0));
        assert_eq!(equal_power_gains(2.0), (0.0, 1.0));

        for step in 0..=1_000 {
            let (outgoing, incoming) = equal_power_gains(step as f32 / 1_000.0);

            assert!((outgoing * outgoing + incoming * incoming - 1.0).abs() < 1.0e-6);
        }
    }

    #[test]
    fn matched_uncorrelated_levels_need_no_adjustment() {
        assert_eq!(target_gain_db(levels(-20.0, -20.0, -20.0), 0.5), 0.0);
    }

    #[test]
    fn correlated_sources_are_attenuated_by_the_measured_excess() {
        let target = target_gain_db(levels(-20.0, -20.0, -17.0), 0.5);

        assert!((target + 3.0).abs() < 1.0e-9, "got {target}");
    }

    #[test]
    fn the_adjustment_never_boosts_even_when_the_mix_measures_quieter() {
        assert_eq!(target_gain_db(levels(-20.0, -20.0, -40.0), 0.5), 0.0);
        assert_eq!(target_gain_db(levels(-60.0, -10.0, -70.0), 0.9), 0.0);
    }

    #[test]
    fn the_adjustment_is_bounded_by_the_maximum_cut() {
        let target = target_gain_db(levels(-10.0, -10.0, 20.0), 0.5);

        assert_eq!(target, -MAX_CUT_DB);
    }

    #[test]
    fn near_silence_disables_the_adjustment() {
        assert_eq!(target_gain_db(levels(-90.0, -10.0, -12.0), 0.5), 0.0);
        assert_eq!(target_gain_db(levels(-10.0, -100.0, -12.0), 0.5), 0.0);
        assert_eq!(target_gain_db(levels(-100.0, -100.0, -100.0), 0.5), 0.0);

        let partial = target_gain_db(levels(-62.0, -10.0, 0.0), 0.5);

        assert!(partial < 0.0 && partial > -MAX_CUT_DB, "got {partial}");
    }

    #[test]
    fn edge_weight_is_zero_at_both_ends_and_one_in_the_middle() {
        let window = 48_000;

        for position in [0.0, 480.0, 960.0, 47_039.0, 47_519.0, 47_999.0] {
            assert_eq!(edge_weight(position, window), 0.0, "at {position}");
        }

        assert_eq!(edge_weight(24_000.0, window), 1.0);

        let mut previous = 0.0;

        for position in (0..24_000).step_by(100) {
            let weight = edge_weight(position as f64, window);

            assert!(weight >= previous, "weight must rise towards the middle");

            previous = weight;
        }
    }

    #[test]
    fn gain_helpers_are_exact_at_unity() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(-6.0) - 0.501_187).abs() < 1.0e-5);
        assert!((power_to_db(1.0)).abs() < 1.0e-12);
        assert_eq!(smoothstep(-1.0), 0.0);
        assert_eq!(smoothstep(2.0), 1.0);
    }

    #[test]
    fn adaptive_gain_without_data_stays_at_unity() {
        let mut gain = AdaptiveGain::new(48_000);

        for hop in 0..10 {
            gain.begin_hop(hop * HOP_FRAMES);

            assert_eq!(gain.ramp(), (1.0, 0.0));
            assert_eq!(gain.deepest_gain_db(), 0.0);
        }
    }
}
