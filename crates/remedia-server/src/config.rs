use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use remedia_core::OpusEncoderConfig;
use remedia_observability::{ObservabilityConfig, ObservabilityError};

const DEFAULT_PORT: u16 = 8787;
const MAX_MESSAGE_BYTES: usize = 1_048_576;
const MAX_CONNECTIONS: usize = 65_536;
const MAX_CLIENT_SESSIONS: usize = 10_000;
const MAX_TOTAL_PLAYERS: usize = 10_000;
const MAX_PLAYERS_PER_CLIENT: usize = 1_000;
const MAX_IN_FLIGHT_REQUESTS: usize = 4_096;
const MAX_REQUESTS_PER_SECOND: usize = 100_000;
const MAX_EVENT_BUFFER: usize = 65_536;

#[derive(Clone)]
pub struct ServerConfig {
    pub(crate) bind_host: String,
    pub(crate) port: u16,
    pub(crate) users: Vec<AuthUser>,
    pub(crate) resume_timeout_seconds: u64,
    pub(crate) limits: Limits,
    pub(crate) media: MediaAccess,
    pub(crate) opus: OpusEncoderConfig,
    pub(crate) observability_console: bool,
}

#[derive(Clone)]
pub(crate) struct AuthUser {
    pub(crate) username: String,
    pub(crate) password: String,
}

#[derive(Clone)]
pub(crate) struct Limits {
    pub(crate) max_connections: usize,
    pub(crate) max_client_sessions: usize,
    pub(crate) max_total_players: usize,
    pub(crate) max_players_per_client: usize,
    pub(crate) max_message_bytes: usize,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_in_flight_requests: usize,
    pub(crate) max_requests_per_second: usize,
    pub(crate) event_buffer: usize,
}

#[derive(Clone)]
pub(crate) struct MediaAccess {
    pub(crate) allowed_file_roots: Vec<PathBuf>,
    pub(crate) allowed_url_hosts: Vec<String>,
    pub(crate) allow_http: bool,
    pub(crate) allow_https: bool,
    pub(crate) block_private_networks: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration file `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("media.allowedFileRoots path `{path}` is unavailable: {source}")]
    MediaRootIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid configuration: {0}")]
    Invalid(&'static str),
    #[error("invalid configuration: {0}")]
    Audio(#[source] remedia_core::OutputError),
    #[error(
        "configuration field `{field}` references unavailable environment variable `{variable}`"
    )]
    MissingEnvironment { field: String, variable: String },
    #[error(
        "configuration field `{0}` has an invalid environment reference; expected env('VARIABLE_NAME')"
    )]
    InvalidEnvironmentReference(String),
    #[error("observability configuration is invalid: {0}")]
    Observability(#[from] ObservabilityError),
    #[error("observability environment configuration is invalid: {0}")]
    ObservabilityConfig(#[from] remedia_observability::ConfigError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    #[serde(default)]
    bind: RawBind,
    users: Vec<RawUser>,
    #[serde(default = "default_resume_timeout")]
    session_resumption_timeout_seconds: u64,
    #[serde(default)]
    limits: RawLimits,
    #[serde(default)]
    media: RawMedia,
    #[serde(default)]
    audio: RawAudio,
    #[serde(default)]
    observability: RawObservability,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawBind {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAudio {
    opus: RawOpus,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawOpus {
    #[serde(deserialize_with = "deserialize_bitrate")]
    bitrate: i32,
    #[serde(deserialize_with = "deserialize_complexity")]
    complexity: i32,
    #[serde(deserialize_with = "deserialize_vbr")]
    vbr: bool,
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

fn deserialize_bitrate<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
    i32::deserialize(deserializer)
        .map_err(|error| serde::de::Error::custom(format!("audio.opus.bitrate: {error}")))
}

fn deserialize_complexity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i32, D::Error> {
    i32::deserialize(deserializer)
        .map_err(|error| serde::de::Error::custom(format!("audio.opus.complexity: {error}")))
}

fn deserialize_vbr<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
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
struct RawUser {
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawLimits {
    max_connections: usize,
    max_client_sessions: usize,
    max_total_players: usize,
    max_players_per_client: usize,
    max_message_bytes: usize,
    request_timeout_ms: u64,
    max_in_flight_requests: usize,
    max_requests_per_second: usize,
    event_buffer: usize,
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
struct RawMedia {
    allowed_file_roots: Vec<String>,
    allowed_url_hosts: Vec<String>,
    allow_http: bool,
    allow_https: bool,
    block_private_networks: bool,
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
struct RawObservability {
    console: bool,
}

impl Default for RawObservability {
    fn default() -> Self {
        Self { console: true }
    }
}

impl ServerConfig {
    /// Load and strictly validate a JSON configuration file.
    ///
    /// Relative media roots are resolved relative to the configuration file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self::parse_with(&contents, base, |key| env::var(key).ok())
    }

    fn parse_with(
        json: &str,
        base: &Path,
        mut get_env: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let raw: RawConfig = serde_json::from_str(json)?;
        let opus = OpusEncoderConfig {
            bitrate: raw.audio.opus.bitrate,
            complexity: raw.audio.opus.complexity,
            vbr: raw.audio.opus.vbr,
        };
        opus.validate().map_err(ConfigError::Audio)?;
        let bind_host = resolve_env("bind.host", raw.bind.host, &mut get_env)?;
        if bind_host.trim().is_empty() {
            return Err(ConfigError::Invalid("bind.host must not be empty"));
        }
        let resume_timeout_seconds = raw.session_resumption_timeout_seconds;
        if resume_timeout_seconds == 0 || resume_timeout_seconds > 86_400 {
            return Err(ConfigError::Invalid(
                "sessionResumptionTimeoutSeconds must be between 1 and 86400",
            ));
        }

        let mut users = Vec::with_capacity(raw.users.len());
        for raw_user in raw.users {
            let username = resolve_env("users[].username", raw_user.username, &mut get_env)?;
            let password = resolve_env("users[].password", raw_user.password, &mut get_env)?;
            if username.is_empty() || username.len() > 128 || username.contains(':') {
                return Err(ConfigError::Invalid(
                    "usernames must be 1 to 128 characters and cannot contain ':'",
                ));
            }
            if password.is_empty() || password.len() > 4096 {
                return Err(ConfigError::Invalid(
                    "user passwords must be 1 to 4096 bytes",
                ));
            }
            if users
                .iter()
                .any(|user: &AuthUser| user.username == username)
            {
                return Err(ConfigError::Invalid("usernames must be unique"));
            }
            users.push(AuthUser { username, password });
        }
        if users.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one authenticated user is required",
            ));
        }

        let raw_limits = raw.limits;
        let limits = Limits {
            max_connections: raw_limits.max_connections,
            max_client_sessions: raw_limits.max_client_sessions,
            max_total_players: raw_limits.max_total_players,
            max_players_per_client: raw_limits.max_players_per_client,
            max_message_bytes: raw_limits.max_message_bytes,
            request_timeout_ms: raw_limits.request_timeout_ms,
            max_in_flight_requests: raw_limits.max_in_flight_requests,
            max_requests_per_second: raw_limits.max_requests_per_second,
            event_buffer: raw_limits.event_buffer,
        };
        if limits.max_connections == 0
            || limits.max_client_sessions == 0
            || limits.max_total_players == 0
            || limits.max_players_per_client == 0
            || limits.max_in_flight_requests == 0
            || limits.max_requests_per_second == 0
            || limits.event_buffer == 0
        {
            return Err(ConfigError::Invalid(
                "resource limits must be greater than zero",
            ));
        }
        if limits.max_connections > tokio::sync::Semaphore::MAX_PERMITS
            || limits.max_in_flight_requests > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(ConfigError::Invalid(
                "connection and in-flight request limits exceed the platform semaphore capacity",
            ));
        }
        if limits.max_connections > MAX_CONNECTIONS
            || limits.max_client_sessions > MAX_CLIENT_SESSIONS
            || limits.max_total_players > MAX_TOTAL_PLAYERS
            || limits.max_players_per_client > MAX_PLAYERS_PER_CLIENT
            || limits.max_in_flight_requests > MAX_IN_FLIGHT_REQUESTS
            || limits.max_requests_per_second > MAX_REQUESTS_PER_SECOND
            || limits.event_buffer > MAX_EVENT_BUFFER
        {
            return Err(ConfigError::Invalid(
                "one or more resource limits exceed their supported maximum",
            ));
        }
        if !(1_024..=MAX_MESSAGE_BYTES).contains(&limits.max_message_bytes) {
            return Err(ConfigError::Invalid(
                "limits.maxMessageBytes must be between 1024 and 1048576",
            ));
        }
        if limits.request_timeout_ms == 0 || limits.request_timeout_ms > 300_000 {
            return Err(ConfigError::Invalid(
                "limits.requestTimeoutMs must be between 1 and 300000",
            ));
        }
        if limits.max_players_per_client > limits.max_total_players {
            return Err(ConfigError::Invalid(
                "maxPlayersPerClient cannot exceed maxTotalPlayers",
            ));
        }

        let mut allowed_file_roots = Vec::new();
        for configured_root in raw.media.allowed_file_roots {
            let configured_root =
                resolve_env("media.allowedFileRoots[]", configured_root, &mut get_env)?;
            let path = expand_home_path(&configured_root)?;
            let path = if path.is_absolute() {
                path
            } else {
                base.join(path)
            };
            let canonical = fs::canonicalize(&path).map_err(|source| ConfigError::MediaRootIo {
                path: path.clone(),
                source,
            })?;
            if !canonical.is_dir() {
                return Err(ConfigError::Invalid(
                    "every media.allowedFileRoots entry must be a directory",
                ));
            }
            if !allowed_file_roots.contains(&canonical) {
                allowed_file_roots.push(canonical);
            }
        }
        let mut allowed_url_hosts = Vec::new();
        for host in raw.media.allowed_url_hosts {
            let host = resolve_env("media.allowedUrlHosts[]", host, &mut get_env)?;
            let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
            if !valid_host_pattern(&host) {
                return Err(ConfigError::Invalid(
                    "media.allowedUrlHosts entries must be host names or `*`",
                ));
            }
            if host == "*" && allowed_url_hosts.iter().any(|item| item != "*") {
                return Err(ConfigError::Invalid(
                    "media.allowedUrlHosts cannot combine `*` with specific hosts",
                ));
            }
            if allowed_url_hosts.contains(&"*".to_owned()) && host != "*" {
                return Err(ConfigError::Invalid(
                    "media.allowedUrlHosts cannot combine `*` with specific hosts",
                ));
            }
            if !allowed_url_hosts.contains(&host) {
                allowed_url_hosts.push(host);
            }
        }

        Ok(Self {
            bind_host,
            port: raw.bind.port,
            users,
            resume_timeout_seconds,
            limits,
            media: MediaAccess {
                allowed_file_roots,
                allowed_url_hosts,
                allow_http: raw.media.allow_http,
                allow_https: raw.media.allow_https,
                block_private_networks: raw.media.block_private_networks,
            },
            observability_console: raw.observability.console,
            opus,
        })
    }

    pub(crate) fn observability_config(&self) -> Result<ObservabilityConfig, ConfigError> {
        Ok(ObservabilityConfig::from_env()?.with_console_enabled(self.observability_console))
    }

    pub(crate) fn authenticate(&self, username: &str, password: &str) -> Option<&AuthUser> {
        use subtle::ConstantTimeEq;

        self.users.iter().find(|user| {
            if user.username != username {
                return false;
            }
            let expected = user.password.as_bytes();
            let provided = password.as_bytes();
            expected.len() == provided.len() && bool::from(expected.ct_eq(provided))
        })
    }

    pub(crate) fn authorize_file(&self, file: &str) -> Result<PathBuf, MediaAccessError> {
        let path = fs::canonicalize(file).map_err(|_| MediaAccessError::NotAllowed)?;
        if self.media.allowed_file_roots.is_empty()
            || !self
                .media
                .allowed_file_roots
                .iter()
                .any(|root| path.starts_with(root))
        {
            return Err(MediaAccessError::NotAllowed);
        }
        if !path.is_file() {
            return Err(MediaAccessError::NotAllowed);
        }
        Ok(path)
    }

    pub(crate) async fn authorize_url(&self, source: &str) -> Result<String, MediaAccessError> {
        let url = url::Url::parse(source).map_err(|_| MediaAccessError::NotAllowed)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(MediaAccessError::NotAllowed);
        }
        let scheme_allowed = match url.scheme() {
            "http" => self.media.allow_http,
            "https" => self.media.allow_https,
            _ => false,
        };
        if !scheme_allowed {
            return Err(MediaAccessError::NotAllowed);
        }
        let host = url.host_str().ok_or(MediaAccessError::NotAllowed)?;
        if !host_allowed(host, &self.media.allowed_url_hosts) {
            return Err(MediaAccessError::NotAllowed);
        }
        let port = url
            .port_or_known_default()
            .ok_or(MediaAccessError::NotAllowed)?;
        if self.media.block_private_networks {
            let addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| MediaAccessError::NotAllowed)?
                .collect::<Vec<_>>();
            if addrs.is_empty() || addrs.iter().any(|address| !is_public_ip(address.ip())) {
                return Err(MediaAccessError::NotAllowed);
            }
        }
        Ok(url.into())
    }
}

#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum MediaAccessError {
    #[error("media source is outside the configured access policy")]
    NotAllowed,
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

const fn default_port() -> u16 {
    DEFAULT_PORT
}

const fn default_resume_timeout() -> u64 {
    60
}

fn resolve_env(
    field: &str,
    value: String,
    get_env: &mut impl FnMut(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    if !value.starts_with("env(") {
        return Ok(value);
    }
    let Some(variable) = value
        .strip_prefix("env('")
        .and_then(|value| value.strip_suffix("')"))
        .filter(|name| valid_env_name(name))
    else {
        return Err(ConfigError::InvalidEnvironmentReference(field.to_owned()));
    };
    get_env(variable).ok_or_else(|| ConfigError::MissingEnvironment {
        field: field.to_owned(),
        variable: variable.to_owned(),
    })
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub(crate) fn parse_snowflake(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn expand_home_path(path: &str) -> Result<PathBuf, ConfigError> {
    let suffix = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
    };
    let Some(suffix) = suffix else {
        return Ok(PathBuf::from(path));
    };
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(ConfigError::Invalid(
            "media.allowedFileRoots uses `~` but the home directory is unavailable",
        ))?;
    Ok(PathBuf::from(home).join(suffix))
}

fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    !allowlist.is_empty()
        && allowlist.iter().any(|allowed| {
            allowed == "*"
                || host.eq_ignore_ascii_case(allowed)
                || host
                    .to_ascii_lowercase()
                    .strip_suffix(&format!(".{allowed}"))
                    .is_some()
        })
}

fn valid_host_pattern(host: &str) -> bool {
    if host == "*" {
        return true;
    }
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [a, b, c, _] = address.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 0 || b == 168) && (b != 0 || c == 0))
                || (a == 192 && b == 0 && c == 2)
                || (a == 198 && ((b == 18) || (b == 19) || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113)
                || (a == 192 && b == 88 && c == 99)
                || a >= 224)
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            (segments[0] & 0xe000) == 0x2000
                && !address.is_unicast_link_local()
                && !address.is_unique_local()
                && !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !(segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
                && segments[0] != 0x2002
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        ConfigError, ServerConfig, host_allowed, is_public_ip, resolve_env, valid_host_pattern,
    };

    const JSON: &str = r#"{
      "users": [{"username":"bot","password":"env('BOT_PASSWORD')"}]
    }"#;

    #[test]
    fn defaults_bind_to_loopback_and_load_limits() {
        let config = ServerConfig::parse_with(JSON, Path::new("."), |key| {
            (key == "BOT_PASSWORD").then(|| "secret".to_owned())
        })
        .expect("valid config");
        assert_eq!(config.bind_host, "127.0.0.1");
        assert_eq!(config.port, 8787);
        assert_eq!(config.limits.max_message_bytes, 65_536);
        assert_eq!(config.users[0].password, "secret");
        assert_eq!(config.opus, remedia_core::OpusEncoderConfig::default());
    }

    #[test]
    fn audio_defaults_and_individual_opus_overrides() {
        use remedia_core::OpusEncoderConfig;
        for (audio, expected) in [
            ("{}", OpusEncoderConfig::default()),
            (r#"{"opus":{}}"#, OpusEncoderConfig::default()),
            (
                r#"{"opus":{"bitrate":96000}}"#,
                OpusEncoderConfig {
                    bitrate: 96_000,
                    ..Default::default()
                },
            ),
            (
                r#"{"opus":{"complexity":0}}"#,
                OpusEncoderConfig {
                    complexity: 0,
                    ..Default::default()
                },
            ),
            (
                r#"{"opus":{"vbr":false}}"#,
                OpusEncoderConfig {
                    vbr: false,
                    ..Default::default()
                },
            ),
            (
                r#"{"opus":{"bitrate":256000,"complexity":10,"vbr":false}}"#,
                OpusEncoderConfig {
                    bitrate: 256_000,
                    complexity: 10,
                    vbr: false,
                },
            ),
        ] {
            let json = JSON.replace("\"users\":", &format!("\"audio\":{audio},\"users\":"));
            let config =
                ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into())).unwrap();
            assert_eq!(config.opus, expected);
        }
    }

    #[test]
    fn invalid_opus_values_report_the_field_and_unknown_fields_are_rejected() {
        for (field, values) in [
            (
                "bitrate",
                vec![
                    "-1",
                    "0",
                    "499",
                    "512001",
                    "4294967296",
                    "1.5",
                    "null",
                    "\"128000\"",
                ],
            ),
            (
                "complexity",
                vec!["-1", "11", "4294967296", "1.5", "null", "true"],
            ),
            ("vbr", vec!["0", "null", "\"true\""]),
        ] {
            for value in values {
                let json = JSON.replace(
                    "\"users\":",
                    &format!("\"audio\":{{\"opus\":{{\"{field}\":{value}}}}},\"users\":"),
                );
                let error =
                    ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into()))
                        .err()
                        .expect("invalid config");
                assert!(
                    error.to_string().contains(&format!("audio.opus.{field}")),
                    "{error}"
                );
            }
        }
        for audio in [
            r#"{"extra":true}"#,
            r#"{"opus":{"fec":true}}"#,
            r#"{"opus":{"channelCount":1}}"#,
        ] {
            let json = JSON.replace("\"users\":", &format!("\"audio\":{audio},\"users\":"));
            let error = ServerConfig::parse_with(&json, Path::new("."), |_| Some("secret".into()))
                .err()
                .expect("unknown field");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn rejects_missing_env_unknown_keys_and_bad_values() {
        assert!(matches!(
            ServerConfig::parse_with(JSON, Path::new("."), |_| None),
            Err(ConfigError::MissingEnvironment { .. })
        ));
        let unknown = JSON.replace("\"users\":", "\"extra\":true,\"users\":");
        assert!(ServerConfig::parse_with(&unknown, Path::new("."), |_| Some("x".into())).is_err());
        let legacy_bot_config = JSON.replace(
            "\"password\":\"env('BOT_PASSWORD')\"",
            "\"password\":\"env('BOT_PASSWORD')\",\"discordBotUserId\":\"123456789012345678\"",
        );
        assert!(
            ServerConfig::parse_with(&legacy_bot_config, Path::new("."), |_| Some("x".into()))
                .is_err()
        );
        let malformed = JSON.replace("env('BOT_PASSWORD')", "env(BOT_PASSWORD)");
        assert!(matches!(
            ServerConfig::parse_with(&malformed, Path::new("."), |_| Some("x".into())),
            Err(ConfigError::InvalidEnvironmentReference(_))
        ));
    }

    #[test]
    fn env_references_are_exact_and_do_not_execute_content() {
        let mut lookup = |name: &str| (name == "VALUE").then(|| "literal $(whoami)".to_owned());
        assert_eq!(
            resolve_env("field", "env('VALUE')".into(), &mut lookup).unwrap(),
            "literal $(whoami)"
        );
        assert!(matches!(
            resolve_env("field", "env('VALUE') suffix".into(), &mut lookup),
            Err(ConfigError::InvalidEnvironmentReference(_))
        ));
    }

    #[test]
    fn home_relative_media_roots_expand_before_resolution() {
        let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
        if let Some(home) = home {
            let home = PathBuf::from(home);
            let json = JSON.replace(
                "\"users\":",
                "\"media\":{\"allowedFileRoots\":[\"~\"]},\"users\":",
            );
            let parsed = ServerConfig::parse_with(&json, Path::new("."), |_| Some("x".into()))
                .expect("home directory is a valid media root");
            assert_eq!(
                super::expand_home_path("~/Music/Downloads").unwrap(),
                home.join("Music/Downloads")
            );
            assert_eq!(
                parsed.media.allowed_file_roots,
                vec![fs::canonicalize(home).unwrap()]
            );
        }
        assert_eq!(
            super::expand_home_path("media").unwrap(),
            PathBuf::from("media")
        );
    }

    #[test]
    fn missing_config_error_reports_the_requested_path() {
        let path = env::temp_dir().join(format!(
            "remedia-missing-config-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        match ServerConfig::from_file(&path) {
            Err(ConfigError::Io { path: reported, .. }) => assert_eq!(reported, path),
            Err(error) => panic!("expected path-aware read error, got {error}"),
            Ok(_) => panic!("missing config path unexpectedly loaded"),
        }
    }

    #[test]
    fn private_and_special_ip_ranges_are_rejected() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.2.3",
            "192.168.1.2",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn remote_hosts_require_explicit_allowlist_or_explicit_wildcard() {
        assert!(!host_allowed("media.example.test", &[]));
        assert!(host_allowed("media.example.test", &["*".into()]));
        assert!(host_allowed("cdn.example.test", &["example.test".into()]));
        assert!(!host_allowed(
            "example-attacker.test",
            &["example.test".into()]
        ));
    }

    #[test]
    fn rejects_malformed_allowed_host_patterns() {
        assert!(valid_host_pattern("media.example.com"));
        assert!(valid_host_pattern("192.0.2.10"));
        assert!(!valid_host_pattern("https://media.example.com"));
        assert!(!valid_host_pattern("-media.example.com"));
        assert!(!valid_host_pattern("media.*.example.com"));
    }
}
