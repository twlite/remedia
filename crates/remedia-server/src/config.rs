use std::collections::HashMap;
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
    pub(crate) crossfade: CrossfadeConfig,
    pub(crate) observability_console: bool,
}

/// Validated crossfade policy. Native playback readiness is negotiated separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CrossfadeConfig {
    pub(crate) enabled: bool,
    pub(crate) duration_ms: u32,
    pub(crate) prepare_before_ms: u32,
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

impl ServerConfig {
    /// Load and strictly validate a JSON configuration file.
    ///
    /// Relative media roots are resolved relative to the configuration file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::from_file_with_lookup(path, |key| env::var(key).ok())
    }

    /// Load configuration using a map of values read from CLI environment files.
    ///
    /// Variables from the current process have precedence over file values.
    /// This does not modify the process-wide environment or expose any new
    /// interpolation syntax beyond env('KEY').
    pub fn from_file_with_env(
        path: impl AsRef<Path>,
        file_values: &HashMap<String, String>,
    ) -> Result<Self, ConfigError> {
        Self::from_file_with_lookup(path, |key| {
            env::var(key).ok().or_else(|| file_values.get(key).cloned())
        })
    }

    fn from_file_with_lookup(
        path: impl AsRef<Path>,
        get_env: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self::parse_with(&contents, base, get_env)
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
        let fade = &raw.audio.crossfade;
        if !(1_000..=30_000).contains(&fade.duration_ms) {
            return Err(ConfigError::Invalid(
                "audio.crossfade.durationMs must be between 1000 and 30000",
            ));
        }
        if !(5_000..=120_000).contains(&fade.prepare_before_ms) {
            return Err(ConfigError::Invalid(
                "audio.crossfade.prepareBeforeMs must be between 5000 and 120000",
            ));
        }
        if fade.curve != "equalPower" {
            return Err(ConfigError::Invalid(
                "audio.crossfade.curve must be equalPower",
            ));
        }
        let crossfade = CrossfadeConfig {
            enabled: fade.enabled,
            duration_ms: fade.duration_ms,
            prepare_before_ms: fade.prepare_before_ms,
        };
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
            crossfade,
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
mod tests;

mod media_access;

mod raw;
use raw::*;
