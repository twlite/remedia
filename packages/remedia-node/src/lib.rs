mod diagnostics;
mod discord;
mod input;
mod native;
mod playback;
mod probe;
mod runtime;
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
