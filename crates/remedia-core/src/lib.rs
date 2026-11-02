//! Native media input and audio decoding for Remedia.
//!
//! This crate opens local paths and HTTP(S) URLs through FFmpeg's native
//! libraries and yields normalized interleaved PCM incrementally.

mod byte_stream;
mod decoder;
mod error;
mod input;
mod opus;
mod output;
mod playback;
mod source;

use remedia_observability::ObservabilityHandle;

pub(crate) fn with_dispatch<T>(handle: &ObservabilityHandle, callback: impl FnOnce() -> T) -> T {
    match handle.dispatch() {
        Some(dispatch) => tracing::dispatcher::with_default(dispatch, callback),
        None => callback(),
    }
}

pub use byte_stream::{ByteStreamError, ByteStreamWriter};
pub use decoder::{AudioDecoder, AudioStreamInfo, DecoderOptions, PcmFormat, PcmFrame};
pub use error::DecoderError;
pub use opus::{EncodedOpusPacket, OpusEncoderOutput, OpusPacketSink};
pub use output::{MediaOutput, NullOutput, NullOutputStats, NullOutputStatsHandle, OutputError};
pub use playback::{PlaybackError, PlaybackPosition, PlaybackSession, PlaybackState};
pub use source::{CancellationToken, InputSource};
