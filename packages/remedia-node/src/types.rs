use napi::bindgen_prelude::Buffer;
use napi_derive::napi;
use std::collections::HashMap;

#[napi(object)]
pub struct NativeVoiceOptions {
    pub guild_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

#[napi(object)]
pub struct NativeVoiceSessionIds {
    pub voice_id: String,
    pub playback_id: String,
}

#[napi(object)]
pub struct NativeMediaSource {
    /// Explicitly `file`, `url`, `buffer`, or `stream`.
    pub kind: String,
    pub value: Option<String>,
    pub bytes: Option<Buffer>,
}

#[napi(object)]
pub struct NativeStreamInfo {
    pub stream_index: u32,
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub channel_layout: Option<String>,
    pub duration_ms: Option<f64>,
    pub bit_rate: Option<f64>,
    pub metadata: HashMap<String, String>,
}

#[napi(object)]
pub struct NativePlaybackPosition {
    pub media_ms: Option<f64>,
    pub media_position_accuracy: Option<String>,
    pub submitted_ms: f64,
    pub played_ms: Option<f64>,
}

#[napi(object)]
pub struct NativeDiagnosticField {
    pub kind: String,
    pub string_value: Option<String>,
    pub bool_value: Option<bool>,
    pub number_value: Option<f64>,
}

#[napi(object)]
pub struct NativeDiagnosticEvent {
    pub timestamp: f64,
    pub severity: String,
    pub target: String,
    pub event: String,
    pub message: Option<String>,
    pub playback_id: Option<String>,
    pub voice_connection_id: Option<String>,
    pub fields: HashMap<String, NativeDiagnosticField>,
}

/// A bounded native stage with an application-owned queue occurrence identity.
#[napi(object)]
pub struct NativeStageSnapshot {
    pub expected_generation: String,
    pub entry_id: String,
    pub queue_revision: String,
    pub source_revision: String,
    pub state: String,
    pub buffered_ms: f64,
    pub info: Option<NativeStreamInfo>,
    pub error: Option<String>,
}

/// One ordered event emitted by the native staged-source worker.
#[napi(object)]
pub struct NativeCrossfadeEvent {
    pub kind: String,
    pub generation: String,
    pub previous_generation: Option<String>,
    pub entry_id: Option<String>,
    pub queue_revision: Option<String>,
    pub source_revision: Option<String>,
    pub remaining_ms: Option<f64>,
    pub reason: Option<String>,
    pub info: Option<NativeStreamInfo>,
}
