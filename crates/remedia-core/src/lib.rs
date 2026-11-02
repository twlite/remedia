//! Native media input and audio decoding for Remedia.
//!
//! This crate opens local paths and HTTP(S) URLs through FFmpeg's native
//! libraries and yields normalized interleaved PCM incrementally.

mod decoder;
mod error;
mod input;
mod output;
mod playback;
mod source;

pub use decoder::{AudioDecoder, AudioStreamInfo, DecoderOptions, PcmFormat, PcmFrame};
pub use error::DecoderError;
pub use output::{MediaOutput, NullOutput, NullOutputStats, NullOutputStatsHandle, OutputError};
pub use playback::{PlaybackError, PlaybackPosition, PlaybackSession, PlaybackState};
pub use source::{CancellationToken, InputSource};
