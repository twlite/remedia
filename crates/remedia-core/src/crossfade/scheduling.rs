//! Conservative endpoint analysis. Reuses the mixer's 10 ms hop size, retains
//! only hop powers, and never treats relative quiet alone as silence.
use super::analysis::HOP_FRAMES;

const MAX_HOPS: usize = 5_200;

pub(crate) struct TailAnalysis {
    powers: Vec<f64>,
    sum: f64,
    frames: usize,
    invalid: bool,
}

impl TailAnalysis {
    pub(crate) fn new() -> Self {
        Self {
            powers: Vec::with_capacity(MAX_HOPS),
            sum: 0.0,
            frames: 0,
            invalid: false,
        }
    }

    pub(crate) fn push(&mut self, pcm: &[f32]) {
        for pair in pcm.as_chunks::<2>().0 {
            self.invalid |= !pair[0].is_finite() || !pair[1].is_finite();
            self.sum += (f64::from(pair[0]).powi(2) + f64::from(pair[1]).powi(2)) * 0.5;
            self.frames += 1;
            if self.frames == HOP_FRAMES {
                if self.powers.len() == MAX_HOPS {
                    self.invalid = true;
                    return;
                }
                self.powers.push(self.sum / HOP_FRAMES as f64);
                self.sum = 0.0;
                self.frames = 0;
            }
        }
    }

    /// Return an endpoint relative to the analyzed region, only after EOF has
    /// been verified by the caller. Keep 250 ms beyond the last audible hop.
    pub(crate) fn endpoint(&self) -> Option<u64> {
        if self.invalid || self.powers.len() < 225 {
            return None;
        }
        let reference = self.powers.iter().copied().fold(0.0_f64, f64::max);
        // At least 45 dB below the observed music and below -65 dBFS.
        // Quiet masters receive a still lower gate, down to -100 dBFS.
        let gate = (reference * 0.000_031_622_776_6).clamp(1e-10, 3.162_277_66e-7);
        if reference < gate * 1000.0 {
            return None;
        }
        let mut smooth = 0.0_f64;
        let mut last = 0;
        let mut audible = 0;
        for (index, power) in self.powers.iter().enumerate() {
            smooth += 0.25 * (power - smooth);
            if *power > gate || smooth > gate {
                last = index + 1;
                audible += 1;
            }
        }
        // A partial final hop can invalidate silence confidence too.
        if self.frames > 0 && self.sum / self.frames as f64 > gate {
            return None;
        }
        if audible < 75 || self.powers.len().saturating_sub(last) < 150 {
            return None;
        }
        Some(((last + 25) * HOP_FRAMES) as u64)
    }
}

/// Leading trim only accepts near-digital silence followed by a clear onset.
/// The caller retains the prefix if confidence or the bounded window runs out.
pub(crate) fn leading_trim(pcm: &[f32]) -> usize {
    let mut silent_hops = 0;
    for hop in pcm.as_chunks::<{ HOP_FRAMES * 2 }>().0 {
        let power = hop.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / hop.len() as f64;
        let peak = hop.iter().copied().map(f32::abs).fold(0.0_f32, f32::max);
        if !power.is_finite() {
            return 0;
        }
        if power <= 1e-10 && peak <= 0.000_03 {
            silent_hops += 1;
            continue;
        }
        // A quiet intro is retained in full. A strong onset confirms padding.
        return if silent_hops >= 30 && power > 1e-6 {
            (silent_hops - 5) * HOP_FRAMES
        } else {
            0
        };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(parts: &[(usize, f32)]) -> Vec<f32> {
        parts
            .iter()
            .flat_map(|(hops, level)| vec![*level; hops * HOP_FRAMES * 2])
            .collect()
    }

    fn end(parts: &[(usize, f32)]) -> Option<u64> {
        let mut analysis = TailAnalysis::new();
        analysis.push(&signal(parts));
        analysis.endpoint()
    }

    #[test]
    fn silent_and_noise_floor_tails_advance_the_endpoint_conservatively() {
        for level in [0.0, 0.000_001] {
            let endpoint = end(&[(300, 0.2), (700, level)]).unwrap();
            assert!((325 * 480..380 * 480).contains(&endpoint));
        }
    }

    #[test]
    fn pauses_quiet_outros_and_ambient_tracks_are_preserved() {
        assert!(end(&[(300, 0.2), (50, 0.0)]).is_none());
        assert!(end(&[(300, 0.2), (300, 0.0), (100, 0.01)]).is_none());
        assert!(end(&[(300, 0.2), (700, 0.001)]).is_none());
        assert!(end(&[(1000, 0.000_1)]).is_none());
        assert!(end(&[(1000, 0.0)]).is_none());
    }

    #[test]
    fn fade_and_reverb_are_kept_until_the_inaudible_floor() {
        let mut pcm = signal(&[(300, 0.2)]);
        for hop in 0..600 {
            pcm.extend(vec![0.2 * (-hop as f32 / 65.0).exp(); 960]);
        }
        pcm.extend(signal(&[(300, 0.0)]));
        let mut analysis = TailAnalysis::new();
        analysis.push(&pcm);
        let endpoint = analysis.endpoint().unwrap();
        // The first ~400 hops of the decay are above the conservative floor.
        assert!(endpoint > 700 * 480);
        assert!(endpoint < 1000 * 480);
    }

    #[test]
    fn analysis_is_chunk_invariant_and_invalid_input_falls_back() {
        let pcm = signal(&[(300, 0.2), (700, 0.0)]);
        let mut whole = TailAnalysis::new();
        whole.push(&pcm);
        let mut split = TailAnalysis::new();
        for block in pcm.chunks(274) {
            split.push(block);
        }
        assert_eq!(whole.endpoint(), split.endpoint());
        split.push(&[f32::NAN, 0.0]);
        assert!(split.endpoint().is_none());
        let mut overflow = TailAnalysis::new();
        overflow.push(&signal(&[(5300, 0.0)]));
        assert!(overflow.endpoint().is_none());
    }

    #[test]
    fn leading_padding_needs_a_clear_onset_and_keeps_preroll() {
        assert_eq!(leading_trim(&signal(&[(100, 0.0), (10, 0.1)])), 95 * 480);
        assert_eq!(leading_trim(&signal(&[(20, 0.0), (10, 0.1)])), 0);
        assert_eq!(leading_trim(&signal(&[(100, 0.0), (10, 0.000_1)])), 0);
        assert_eq!(leading_trim(&signal(&[(100, 0.000_1), (10, 0.1)])), 0);
        assert_eq!(leading_trim(&signal(&[(200, 0.0)])), 0);
    }
}
