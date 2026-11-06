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

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlaySource {
    File { path: String },
    Url { url: String },
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
