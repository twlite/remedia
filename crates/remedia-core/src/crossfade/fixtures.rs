//! Deterministic synthetic programme material and loudness metrics.
//!
//! This file is deliberately self-contained (no `crate::` or `super::` paths)
//! so that the unit tests and `examples/crossfade_adaptive.rs` share exactly the
//! same signals. The generators use only a seeded integer PRNG and `f64` math,
//! so fixtures are reproducible from their parameters alone.
//!
//! The material is music-like rather than a test tone: a kick on the beat, a
//! bass line, a three-note pad with slow movement, and noise hi-hats, panned
//! across the stereo field and scaled to a requested RMS level. Loud masters
//! are hard-limited at the requested peak, as real masters are.

#![allow(dead_code)]

/// The only sample rate Remedia's mixer runs at.
pub const SAMPLE_RATE: usize = 48_000;

/// A small, fast, seeded PRNG (xorshift32).
pub struct Noise(u32);

impl Noise {
    pub fn new(seed: u32) -> Self {
        Self(seed.max(1))
    }

    /// The next value in `-1.0..1.0`.
    pub fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;

        f64::from(self.0) / f64::from(u32::MAX) * 2.0 - 1.0
    }
}

/// Parameters of a synthetic track.
#[derive(Clone, Copy, Debug)]
pub struct TrackSpec {
    pub seed: u32,
    /// Target RMS of the whole track in dBFS.
    pub rms_dbfs: f64,
    /// Magnitude the track is hard-limited to after scaling.
    pub peak: f64,
    pub bpm: f64,
    /// Root note of the pad, in hertz.
    pub root_hz: f64,
}

impl TrackSpec {
    pub const fn new(seed: u32, rms_dbfs: f64, bpm: f64, root_hz: f64) -> Self {
        Self {
            seed,
            rms_dbfs,
            peak: 0.98,
            bpm,
            root_hz,
        }
    }
}

/// Render `frames` of interleaved stereo programme material.
pub fn music(spec: TrackSpec, frames: usize) -> Vec<f32> {
    let rate = SAMPLE_RATE as f64;
    let beat = 60.0 / spec.bpm * rate;
    let mut noise = Noise::new(spec.seed);
    let mut samples = vec![0.0_f64; frames * 2];
    let mut previous_hat = 0.0;
    let tau = std::f64::consts::TAU;

    for frame in 0..frames {
        let t = frame as f64 / rate;
        let since_beat = (frame as f64 % beat) / rate;
        let since_half_beat = (frame as f64 % (beat / 2.0)) / rate;
        let beat_index = (frame as f64 / beat) as u64;

        // Kick: an exponentially decaying sine with a downward pitch sweep.
        let sweep = 45.0 * since_beat + 65.0 * 0.03 * (1.0 - (-since_beat / 0.03).exp());
        let kick = (tau * sweep).sin() * (-since_beat / 0.09).exp();

        // Bass: root an octave down, sustained and re-articulated every beat.
        let bass_phase = tau * spec.root_hz / 2.0 * t;
        let bass = (bass_phase.sin() + 0.4 * (2.0 * bass_phase).sin())
            * (0.55 + 0.45 * (-since_beat / 0.4).exp());

        // Pad: root, major third, fifth and octave, drifting slowly in level.
        let drift = 0.8 + 0.2 * (tau * 0.23 * t + f64::from(spec.seed)).sin();
        let mut pad_left = 0.0;
        let mut pad_right = 0.0;

        for (index, ratio) in [1.0, 1.26, 1.498, 2.0].into_iter().enumerate() {
            let hz = spec.root_hz
                * ratio
                * if beat_index.is_multiple_of(8) {
                    1.0
                } else {
                    1.122
                };
            let tone = (tau * hz * t).sin() + 0.3 * (tau * 2.0 * hz * t).sin();
            let pan = 0.2 + 0.2 * index as f64;

            pad_left += tone * (1.0 - pan);
            pad_right += tone * pan;
        }

        // Hi-hat: differentiated noise bursts on the off-beat.
        let hat_source = noise.next();
        let hat = (hat_source - previous_hat) * (-since_half_beat / 0.03).exp();

        previous_hat = hat_source;

        let hat_pan = if ((frame as f64 / (beat / 2.0)) as u64).is_multiple_of(2) {
            0.4
        } else {
            0.6
        };

        // Soft saturation keeps the crest factor of a mastered track.
        samples[frame * 2] =
            (0.5 * kick + 0.45 * bass + 0.3 * drift * pad_left + 0.3 * hat * (1.0 - hat_pan) * 2.0)
                .tanh();
        samples[frame * 2 + 1] =
            (0.5 * kick + 0.45 * bass + 0.3 * drift * pad_right + 0.3 * hat * hat_pan * 2.0).tanh();
    }

    let power = samples.iter().map(|sample| sample * sample).sum::<f64>() / samples.len() as f64;
    let scale = if power > 0.0 {
        10f64.powf(spec.rms_dbfs / 20.0) / power.sqrt()
    } else {
        0.0
    };

    samples
        .into_iter()
        .map(|sample| (sample * scale).clamp(-spec.peak, spec.peak) as f32)
        .collect()
}

/// `frames` of digital silence.
pub fn silence(frames: usize) -> Vec<f32> {
    vec![0.0; frames * 2]
}

/// Uncorrelated stereo noise at `rms_dbfs`, such as a dithered tail.
pub fn noise_floor(seed: u32, rms_dbfs: f64, frames: usize) -> Vec<f32> {
    let mut noise = Noise::new(seed);
    // Uniform noise has an RMS of 1/sqrt(3) of its peak.
    let amplitude = 10f64.powf(rms_dbfs / 20.0) * 3f64.sqrt();

    (0..frames * 2)
        .map(|_| (noise.next() * amplitude) as f32)
        .collect()
}

/// Overwrite single frames at `positions` with a full-scale click of `level`.
pub fn with_clicks(mut signal: Vec<f32>, positions: &[usize], level: f32) -> Vec<f32> {
    for &position in positions {
        signal[position * 2] = level;
        signal[position * 2 + 1] = -level;
    }

    signal
}

/// Mean-square level of interleaved stereo audio in dBFS.
pub fn rms_dbfs(signal: &[f32]) -> f64 {
    if signal.is_empty() {
        return f64::NEG_INFINITY;
    }

    let power = signal
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / signal.len() as f64;

    10.0 * power.max(1.0e-12).log10()
}

/// Largest magnitude in the signal.
pub fn peak(signal: &[f32]) -> f32 {
    signal
        .iter()
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

/// RMS in dBFS of consecutive windows, each `hop` frames apart.
pub fn rms_series_dbfs(signal: &[f32], window: usize, hop: usize) -> Vec<f64> {
    let frames = signal.len() / 2;
    let mut series = Vec::new();
    let mut start = 0;

    while start + window <= frames {
        series.push(rms_dbfs(&signal[start * 2..(start + window) * 2]));

        start += hop;
    }

    series
}

/// The largest absolute change between consecutive entries of a series.
pub fn max_step(series: &[f64]) -> f64 {
    series
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f64::max)
}

/// Correlation of the left and right channels, `-1.0..=1.0`.
pub fn stereo_correlation(signal: &[f32]) -> f64 {
    let (mut cross, mut left_power, mut right_power) = (0.0, 0.0, 0.0);

    for frame in signal.as_chunks::<2>().0 {
        let (left, right) = (f64::from(frame[0]), f64::from(frame[1]));

        cross += left * right;
        left_power += left * left;
        right_power += right * right;
    }

    cross / (left_power * right_power).sqrt().max(1.0e-12)
}

/// Short-term loudness in dBFS: 400 ms windows every 100 ms.
pub fn loudness(signal: &[f32]) -> Vec<f64> {
    rms_series_dbfs(signal, SAMPLE_RATE * 4 / 10, SAMPLE_RATE / 10)
}

/// The mean distance, in decibels, between the loudness of `mixed` and the
/// ideal trajectory that glides from the outgoing track's own level to the
/// incoming track's own level along a smoothstep.
pub fn trajectory_error(mixed: &[f32], outgoing: &[f32], incoming: &[f32]) -> f64 {
    let mixed = loudness(mixed);
    let outgoing = loudness(outgoing);
    let incoming = loudness(incoming);
    let count = mixed.len();
    let mut total = 0.0;

    for (index, level) in mixed.iter().enumerate() {
        let progress = (index as f64 + 2.0) / (count as f64 + 3.0);
        let blend = progress * progress * (3.0 - 2.0 * progress);
        let ideal = (1.0 - blend) * outgoing[index] + blend * incoming[index];

        total += (level - ideal).abs();
    }

    total / count as f64
}
