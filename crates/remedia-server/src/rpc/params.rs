use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ResumeParams {
    pub(super) session_id: String,
    pub(super) resume_credential: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct GuildParams {
    pub(super) guild_id: String,
}

/// Per-guild crossfade request. An explicit opt-in is permitted only when
/// the server policy allows it AND native staged-source playback is available.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct GuildCreateParams {
    pub(super) guild_id: String,
    #[serde(default)]
    pub(super) crossfade: Option<SessionCrossfadeOptions>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct SessionCrossfadeOptions {
    pub(super) enabled: bool,
    #[serde(default)]
    pub(super) duration_ms: Option<u32>,
    #[serde(default)]
    pub(super) prepare_before_ms: Option<u32>,
    #[serde(default)]
    pub(super) curve: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct VoiceConnectParams {
    pub(super) guild_id: String,
    pub(super) channel_id: String,
    pub(super) session_id: String,
    pub(super) token: String,
    pub(super) endpoint: String,
    #[serde(default)]
    pub(super) transport_generation: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct VoiceOperationParams {
    pub(super) guild_id: String,
    #[serde(default)]
    pub(super) transport_generation: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct PlayParams {
    pub(super) guild_id: String,
    pub(super) source: PlaySource,
}

/// A media source selected by the client.
///
/// `Operation` addresses a configured plugin-host deployment by its stable `hostId`.
/// `input` is opaque JSON owned by the consuming application; the server forwards it
/// unchanged and never interprets provider-specific references.
#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum PlaySource {
    File {
        path: String,
    },
    Url {
        url: String,
    },
    Operation {
        host_id: String,
        plugin_id: String,
        operation: String,
        input: Value,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct StageNextParams {
    pub(super) guild_id: String,
    pub(super) source: PlaySource,
    pub(super) expected_generation: u64,
    pub(super) entry_id: String,
    pub(super) queue_revision: u64,
    pub(super) source_revision: u64,
    pub(super) expires_at_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct PrepareAckParams {
    pub(super) guild_id: String,
    pub(super) expected_generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct SeekParams {
    pub(super) guild_id: String,
    pub(super) position_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct FilterSetParams {
    pub(super) guild_id: String,
    pub(super) graph: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct VolumeParams {
    pub(super) guild_id: String,
    pub(super) gain: f64,
}
