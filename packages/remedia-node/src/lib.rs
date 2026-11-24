mod diagnostics;
mod discord;
mod input;
mod native;
mod playback;
mod probe;
mod runtime;
mod stream_output;
#[cfg(test)]
mod tests;
mod types;
mod util;

pub use native::NativeRemedia;
pub use types::{
    NativeDiagnosticEvent, NativeDiagnosticField, NativeMediaSource, NativePlaybackPosition,
    NativeStreamInfo, NativeVoiceOptions, NativeVoiceSessionIds,
};

pub(crate) const NAPI_DIAGNOSTIC_QUEUE_CAPACITY: usize = 128;
pub(crate) const DIAGNOSTIC_RECEIVE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(25);
pub(crate) type NativeResult<T> = napi::Result<T>;

/// Report Remedia version/commit and the actual bundled FFmpeg version, license,
/// configuration and library ABI versions. No playback resources are created.
///
/// @returns JSON containing build identity and the linked FFmpeg metadata.
#[napi_derive::napi]
pub fn bundled_ffmpeg_info() -> String {
    let (version, license, configuration) = remedia_core::ffmpeg_build_info();
    let (decoders, filters, protocols) = remedia_core::ffmpeg_capabilities();
    serde_json::json!({
        "capabilities": { "decoders": decoders, "filters": filters, "protocols": protocols },
        "remediaVersion": env!("REMEDIA_BUILD_VERSION"),
        "remediaCommit": env!("REMEDIA_BUILD_COMMIT"),
        "version": version,
        "license": license,
        "configuration": configuration,
        "libraries": remedia_core::ffmpeg_runtime_versions(),
    })
    .to_string()
}
