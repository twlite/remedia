//! Native media input and audio decoding for Remedia.
//!
//! This crate opens local paths and HTTP(S) URLs through FFmpeg's native
//! libraries and yields normalized interleaved PCM incrementally.

mod byte_stream;
mod crossfade;
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
pub use crossfade::{CrossfadeWindow, MixError, PcmCrossfade, StageBufferError, StagedPcmBuffer};
pub use decoder::{
    AudioDecoder, AudioStreamInfo, DecoderOptions, PcmFormat, PcmFrame, ProbeOptions, probe,
};
pub use error::DecoderError;
pub use filter::FilterGraphError;
pub use opus::{EncodedOpusPacket, OpusEncoderConfig, OpusEncoderOutput, OpusPacketSink};
pub use output::{
    EncodedOpusOutput, MediaOutput, NullOutput, NullOutputStats, NullOutputStatsHandle, OutputError,
};
pub use playback::{
    CrossfadeConfig, CrossfadeEvent, MediaPosition, MediaPositionAccuracy, PlaybackError,
    PlaybackPosition, PlaybackProcessingMode, PlaybackSession, PlaybackState, PlaybackWaiter,
    StageIdentity, StageSnapshot, StageState,
};
pub use source::{CancellationToken, InputSource};

/// Report the runtime versions of the five FFmpeg libraries linked by Remedia.
///
/// Each returned version is FFmpeg's packed `(major << 16) | (minor << 8) | micro`
/// value, not the executable version. Useful for diagnosing ABI mismatches.
pub fn ffmpeg_runtime_versions() -> [(&'static str, u32); 5] {
    // These are thread-safe, side-effect-free version functions in FFmpeg's C API.
    unsafe {
        [
            ("avcodec", ffmpeg_next::ffi::avcodec_version()),
            ("avformat", ffmpeg_next::ffi::avformat_version()),
            ("avutil", ffmpeg_next::ffi::avutil_version()),
            ("avfilter", ffmpeg_next::ffi::avfilter_version()),
            ("swresample", ffmpeg_next::ffi::swresample_version()),
        ]
    }
}
