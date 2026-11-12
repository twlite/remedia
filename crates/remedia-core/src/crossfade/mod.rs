//! Bounded, sample-accurate crossfade math for normalized interleaved stereo PCM.
//!
//! The coordinator is responsible for opening sources, scheduling, and advancing
//! the output's continuous sample clock. This module never opens an input,
//! allocates a decoder, or writes to a Discord transport.
//!
//! - `mixer`: sample-accurate mixing of the two sources.
//! - `curves`: the equal-power and adaptive gain curves.
//! - `analysis`: short-window RMS and loudness estimation for the overlap.
//! - `limiter`: peak management for the mixed output.
//! - `buffer`: the bounded buffer holding the staged incoming source.

mod analysis;
mod buffer;
mod curves;
mod limiter;
mod mixer;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

pub use buffer::{StageBufferError, StagedPcmBuffer};
pub use curves::CrossfadeCurve;
pub use mixer::{MixError, MixStats, PcmCrossfade};

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
