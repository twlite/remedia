//! Native media input and audio decoding for Remedia.
//!
//! This crate opens local paths and HTTP(S) URLs through FFmpeg's native
//! libraries and yields normalized interleaved PCM incrementally.

mod byte_stream;
mod decoder;
mod error;
mod ffmpeg_logging;
mod filter;
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
pub use filter::FilterGraphError;
pub use opus::{EncodedOpusPacket, OpusEncoderOutput, OpusPacketSink};
pub use output::{
    EncodedOpusOutput, MediaOutput, NullOutput, NullOutputStats, NullOutputStatsHandle, OutputError,
};
pub use playback::{
    MediaPosition, MediaPositionAccuracy, PlaybackError, PlaybackPosition, PlaybackProcessingMode,
    PlaybackSession, PlaybackState, PlaybackWaiter,
};
pub use source::{CancellationToken, InputSource};
