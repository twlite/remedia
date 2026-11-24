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
mod stream_output;
pub use stream_output::{StreamChunk, StreamFormat, StreamOutput, StreamPacing, StreamReader};
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
pub use crossfade::{
    CrossfadeCurve, CrossfadeWindow, MixError, MixStats, PcmCrossfade, StageBufferError,
    StagedPcmBuffer,
};
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
    CrossfadeConfig, CrossfadeEvent, MediaPosition, MediaPositionAccuracy, PendingSeek,
    PlaybackError, PlaybackPosition, PlaybackProcessingMode, PlaybackSession, PlaybackState,
    PlaybackWaiter, SeekCanceller, SeekOutcome, StageIdentity, StageSnapshot, StageState,
};
pub use source::{CancellationToken, InputSource, ReopenError, SourceReopener};

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

/// Build identity reported by the linked FFmpeg itself (not a build-time assumption).
///
/// Returns the release version, license and configure command. All strings are owned;
/// this function is side-effect-free and does not initialize playback.
pub fn ffmpeg_build_info() -> (String, String, String) {
    // SAFETY: FFmpeg returns non-null, immutable, process-lifetime C strings.
    unsafe {
        let string = |pointer| {
            std::ffi::CStr::from_ptr(pointer)
                .to_string_lossy()
                .into_owned()
        };
        (
            string(ffmpeg_next::ffi::av_version_info()),
            string(ffmpeg_next::ffi::avutil_license()),
            string(ffmpeg_next::ffi::avutil_configuration()),
        )
    }
}

/// Enumerate bundled audio decoders, filters and input protocols.
///
/// The returned names describe the actual linked build, not an assumed feature list.
/// Enumeration does not open media or allocate playback resources.
pub fn ffmpeg_capabilities() -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut decoders = Vec::new();
    let mut filters = Vec::new();
    let mut protocols = Vec::new();
    // SAFETY: FFmpeg's iteration APIs return immutable process-lifetime descriptors.
    // Each iterator owns a separate opaque cursor initialized to null. Descriptor names
    // are valid NUL-terminated strings and are copied before returning.
    unsafe {
        let mut cursor = std::ptr::null_mut();
        loop {
            let codec = ffmpeg_next::ffi::av_codec_iterate(&mut cursor);
            if codec.is_null() {
                break;
            }
            if (*codec).type_ == ffmpeg_next::ffi::AVMediaType::AVMEDIA_TYPE_AUDIO
                && ffmpeg_next::ffi::av_codec_is_decoder(codec) != 0
            {
                decoders.push(
                    std::ffi::CStr::from_ptr((*codec).name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        cursor = std::ptr::null_mut();
        loop {
            let filter = ffmpeg_next::ffi::av_filter_iterate(&mut cursor);
            if filter.is_null() {
                break;
            }
            filters.push(
                std::ffi::CStr::from_ptr((*filter).name)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        cursor = std::ptr::null_mut();
        loop {
            let protocol = ffmpeg_next::ffi::avio_enum_protocols(&mut cursor, 0);
            if protocol.is_null() {
                break;
            }
            protocols.push(
                std::ffi::CStr::from_ptr(protocol)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    (decoders, filters, protocols)
}

#[cfg(feature = "experimental-scheduler")]
pub use playback::{PlaybackScheduler, SchedulerDiagnostics};
