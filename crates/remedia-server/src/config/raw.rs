use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawConfig {
    #[serde(default)]
    pub(super) bind: RawBind,
    pub(super) users: Vec<RawUser>,
    #[serde(default = "default_resume_timeout")]
    pub(super) session_resumption_timeout_seconds: u64,
    #[serde(default)]
    pub(super) limits: RawLimits,
    #[serde(default)]
    pub(super) media: RawMedia,
    #[serde(default)]
    pub(super) audio: RawAudio,
    #[serde(default)]
    pub(super) observability: RawObservability,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawBind {
    #[serde(default = "default_host")]
    pub(super) host: String,
    #[serde(default = "default_port")]
    pub(super) port: u16,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RawAudio {
    pub(super) opus: RawOpus,
    pub(super) crossfade: RawCrossfade,
}

/// Server-side permission and defaults. This does not enable the playback
/// pipeline until the native staged-source engine is available.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawCrossfade {
    pub(super) enabled: bool,
    pub(super) duration_ms: u32,
    pub(super) prepare_before_ms: u32,
    pub(super) curve: String,
}

impl Default for RawCrossfade {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: 5_000,
            prepare_before_ms: 30_000,
            curve: "equalPower".into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RawOpus {
    #[serde(deserialize_with = "deserialize_bitrate")]
    pub(super) bitrate: i32,
    #[serde(deserialize_with = "deserialize_complexity")]
    pub(super) complexity: i32,
    #[serde(deserialize_with = "deserialize_vbr")]
    pub(super) vbr: bool,
}

impl Default for RawOpus {
    fn default() -> Self {
        let config = OpusEncoderConfig::default();
        Self {
            bitrate: config.bitrate,
            complexity: config.complexity,
            vbr: config.vbr,
        }
    }
}

pub(super) fn deserialize_bitrate<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i32, D::Error> {
    i32::deserialize(deserializer)
        .map_err(|error| serde::de::Error::custom(format!("audio.opus.bitrate: {error}")))
}

pub(super) fn deserialize_complexity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i32, D::Error> {
    i32::deserialize(deserializer)
        .map_err(|error| serde::de::Error::custom(format!("audio.opus.complexity: {error}")))
}

pub(super) fn deserialize_vbr<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    bool::deserialize(deserializer)
        .map_err(|error| serde::de::Error::custom(format!("audio.opus.vbr: {error}")))
}

impl Default for RawBind {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawUser {
    pub(super) username: String,
    pub(super) password: String,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawLimits {
    pub(super) max_connections: usize,
    pub(super) max_client_sessions: usize,
    pub(super) max_total_players: usize,
    pub(super) max_players_per_client: usize,
    pub(super) max_message_bytes: usize,
    pub(super) request_timeout_ms: u64,
    pub(super) max_in_flight_requests: usize,
    pub(super) max_requests_per_second: usize,
    pub(super) event_buffer: usize,
}

impl Default for RawLimits {
    fn default() -> Self {
        Self {
            max_connections: 128,
            max_client_sessions: 128,
            max_total_players: 256,
            max_players_per_client: 16,
            max_message_bytes: 65_536,
            request_timeout_ms: 30_000,
            max_in_flight_requests: 16,
            max_requests_per_second: 60,
            event_buffer: 128,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawMedia {
    pub(super) allowed_file_roots: Vec<String>,
    pub(super) allowed_url_hosts: Vec<String>,
    pub(super) allow_http: bool,
    pub(super) allow_https: bool,
    pub(super) block_private_networks: bool,
}

impl Default for RawMedia {
    fn default() -> Self {
        Self {
            allowed_file_roots: Vec::new(),
            allowed_url_hosts: Vec::new(),
            allow_http: true,
            allow_https: true,
            block_private_networks: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawObservability {
    pub(super) console: bool,
}

impl Default for RawObservability {
    fn default() -> Self {
        Self { console: true }
    }
}
