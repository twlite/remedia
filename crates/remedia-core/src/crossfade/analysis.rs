//! Short-window RMS and smoothed loudness estimation for an overlap.
//!
//! The mixer accumulates squared samples for the outgoing source, the incoming
//! source, and the unadjusted mix into [`HopSums`]. Every [`HOP_FRAMES`] frames
//! (10 ms at 48 kHz) the window is closed, converted to a mean-square power,
//! and fed to a smoothed [`LevelTracker`]. Hops are aligned to the start of the
//! overlap rather than to PCM block boundaries, so the estimates, and every
//! gain derived from them, are identical for any chunking of the audio.
//!
//! Nothing here allocates, and the per-frame cost is three multiply-adds.

use crate::decoder::OUTPUT_SAMPLE_RATE_HZ;

use super::curves::power_to_db;

/// Frames per analysis hop: 10 ms at 48 kHz.
pub(super) const HOP_FRAMES: usize = 480;

/// Time constant for rising energy, in seconds. Slow enough that one loud hit
/// barely moves the estimate.
const ATTACK_SECONDS: f64 = 0.15;

/// Time constant for falling energy, in seconds.
const RELEASE_SECONDS: f64 = 0.3;

/// The most a single hop may raise the energy fed to the smoother, relative to
/// the current estimate. A sudden transient therefore cannot dominate it.
const GROWTH_LIMIT: f64 = 4.0;

/// Additive allowance to [`GROWTH_LIMIT`], so a track can still rise out of
/// digital silence.
const GROWTH_FLOOR: f64 = 1.0e-9;

/// Squared-sample sums for the hop in progress.
///
/// Each sum adds `left² + right²`, which links the channels: one level drives
/// one gain, so stereo balance cannot be disturbed.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HopSums {
    pub(super) outgoing: f32,
    pub(super) incoming: f32,
    pub(super) mix: f32,
}

/// Smoothed levels in decibels relative to full scale (mean-square power).
#[derive(Clone, Copy, Debug)]
pub(super) struct OverlapLevels {
    pub(super) outgoing_db: f64,
    pub(super) incoming_db: f64,
    pub(super) mix_db: f64,
}

/// Smoothed mean-square power of one signal.
#[derive(Clone, Copy, Debug, Default)]
struct LevelTracker {
    power: f64,
    primed: bool,
}

impl LevelTracker {
    fn update(&mut self, hop_power: f64, attack: f64, release: f64) {
        // Overflowed or invalid sums must not poison the estimate.
        let hop_power = if hop_power.is_finite() {
            hop_power.max(0.0)
        } else {
            0.0
        };

        if !self.primed {
            self.power = hop_power;
            self.primed = true;

            return;
        }

        let admitted = hop_power.min(self.power * GROWTH_LIMIT + GROWTH_FLOOR);
        let coefficient = if admitted > self.power {
            attack
        } else {
            release
        };

        self.power += coefficient * (admitted - self.power);
    }
}

/// Level estimation for one overlap. Holds a fixed amount of state.
#[derive(Clone, Debug)]
pub(super) struct OverlapAnalyzer {
    pub(super) sums: HopSums,
    outgoing: LevelTracker,
    incoming: LevelTracker,
    mix: LevelTracker,
    attack: f64,
    release: f64,
    closed_hops: u64,
}

impl OverlapAnalyzer {
    pub(super) fn new() -> Self {
        let hop_seconds = HOP_FRAMES as f64 / f64::from(OUTPUT_SAMPLE_RATE_HZ);

        Self {
            sums: HopSums::default(),
            outgoing: LevelTracker::default(),
            incoming: LevelTracker::default(),
            mix: LevelTracker::default(),
            attack: 1.0 - (-hop_seconds / ATTACK_SECONDS).exp(),
            release: 1.0 - (-hop_seconds / RELEASE_SECONDS).exp(),
            closed_hops: 0,
        }
    }

    /// Fold the finished hop into the smoothed estimates and start a new one.
    pub(super) fn close_hop(&mut self) {
        // Two channels contribute to every frame's sum.
        let scale = 1.0 / (2.0 * HOP_FRAMES as f64);
        let sums = std::mem::take(&mut self.sums);

        self.outgoing
            .update(f64::from(sums.outgoing) * scale, self.attack, self.release);
        self.incoming
            .update(f64::from(sums.incoming) * scale, self.attack, self.release);
        self.mix
            .update(f64::from(sums.mix) * scale, self.attack, self.release);
        self.closed_hops += 1;
    }

    /// The smoothed levels, or `None` before the first hop has closed.
    pub(super) fn levels(&self) -> Option<OverlapLevels> {
        (self.closed_hops > 0).then(|| OverlapLevels {
            outgoing_db: power_to_db(self.outgoing.power),
            incoming_db: power_to_db(self.incoming.power),
            mix_db: power_to_db(self.mix.power),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `hops` identical hops of a constant-amplitude signal.
    fn feed(analyzer: &mut OverlapAnalyzer, hops: usize, outgoing: f32, incoming: f32, mix: f32) {
        for _ in 0..hops {
            let frames = HOP_FRAMES as f32;

            analyzer.sums.outgoing += outgoing * outgoing * 2.0 * frames;
            analyzer.sums.incoming += incoming * incoming * 2.0 * frames;
            analyzer.sums.mix += mix * mix * 2.0 * frames;
            analyzer.close_hop();
        }
    }

    #[test]
    fn levels_are_absent_until_a_hop_closes() {
        let mut analyzer = OverlapAnalyzer::new();

        assert!(analyzer.levels().is_none());

        feed(&mut analyzer, 1, 0.1, 0.1, 0.1);

        assert!(analyzer.levels().is_some());
    }

    #[test]
    fn first_hop_primes_the_estimate_to_its_rms() {
        let mut analyzer = OverlapAnalyzer::new();

        feed(&mut analyzer, 1, 0.1, 0.01, 0.1);

        let levels = analyzer.levels().unwrap();

        assert!((levels.outgoing_db + 20.0).abs() < 0.01);
        assert!((levels.incoming_db + 40.0).abs() < 0.01);
        assert!((levels.mix_db + 20.0).abs() < 0.01);
    }

    #[test]
    fn estimate_converges_to_a_new_steady_level() {
        let mut analyzer = OverlapAnalyzer::new();

        feed(&mut analyzer, 5, 0.01, 0.01, 0.01);
        feed(&mut analyzer, 300, 0.1, 0.1, 0.1);

        let levels = analyzer.levels().unwrap();

        assert!((levels.outgoing_db + 20.0).abs() < 0.1, "{levels:?}");
    }

    #[test]
    fn a_single_loud_hop_barely_moves_the_estimate() {
        let mut analyzer = OverlapAnalyzer::new();

        feed(&mut analyzer, 50, 0.05, 0.05, 0.05);

        let before = analyzer.levels().unwrap().outgoing_db;

        feed(&mut analyzer, 1, 1.0, 0.05, 0.05);

        let after = analyzer.levels().unwrap().outgoing_db;

        assert!(
            after - before < 1.0,
            "one 10 ms burst moved the estimate by {} dB",
            after - before
        );
    }

    #[test]
    fn silence_reads_as_the_floor_without_producing_nan() {
        let mut analyzer = OverlapAnalyzer::new();

        feed(&mut analyzer, 20, 0.0, 0.0, 0.0);

        let levels = analyzer.levels().unwrap();

        assert!(levels.outgoing_db.is_finite() && levels.outgoing_db <= -99.0);
        assert!(levels.mix_db.is_finite());
    }

    #[test]
    fn the_estimate_can_rise_out_of_digital_silence() {
        let mut analyzer = OverlapAnalyzer::new();

        feed(&mut analyzer, 10, 0.0, 0.0, 0.0);
        feed(&mut analyzer, 400, 0.0, 0.1, 0.0);

        let levels = analyzer.levels().unwrap();

        assert!(levels.incoming_db > -21.0, "{levels:?}");
    }

    #[test]
    fn overflowed_sums_are_ignored() {
        let mut analyzer = OverlapAnalyzer::new();

        feed(&mut analyzer, 10, 0.1, 0.1, 0.1);

        analyzer.sums.outgoing = f32::INFINITY;
        analyzer.sums.incoming = f32::NAN;
        analyzer.close_hop();

        let levels = analyzer.levels().unwrap();

        assert!(levels.outgoing_db.is_finite());
        assert!(levels.incoming_db.is_finite());
    }
}
