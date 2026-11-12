//! FFmpeg-free configuration model, plugin-host protocol and discovery transport.
//!
//! The Remedia server and its lightweight CLI both depend on this crate. It must never
//! depend on `remedia-core`, `remedia-discord` or any native media library, so the CLI
//! keeps working on machines without FFmpeg. The server layers native-only validation
//! (such as Opus encoder initialization) on top of what is validated here.

use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

pub mod edit;
pub mod error;
pub mod protocol;
pub mod transport;

mod plugin_hosts;
mod raw;

pub use plugin_hosts::{
    AllowedOperation, PluginHostConfig, PluginHostLimits, Secret, parse_origin, valid_identifier,
};
use raw::RawConfig;

const MAX_MESSAGE_BYTES: usize = 1_048_576;
const MAX_CONNECTIONS: usize = 65_536;
const MAX_CLIENT_SESSIONS: usize = 10_000;
const MAX_TOTAL_PLAYERS: usize = 10_000;
const MAX_PLAYERS_PER_CLIENT: usize = 1_000;
const MAX_IN_FLIGHT_REQUESTS: usize = 4_096;
const MAX_REQUESTS_PER_SECOND: usize = 100_000;
const MAX_EVENT_BUFFER: usize = 65_536;

/// Configuration validation failures that do not involve native libraries.
#[derive(Debug, Error)]
pub enum ConfigError {
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
    #[error(
        "configuration field `{field}` references unavailable environment variable `{variable}`"
    )]
    MissingEnvironment { field: String, variable: String },
    #[error(
        "configuration field `{0}` has an invalid environment reference; expected env('VARIABLE_NAME')"
    )]
    InvalidEnvironmentReference(String),
}

type Lookup<'a> = Box<dyn FnMut(&str) -> Option<String> + 'a>;

/// Resolves `env('NAME')` references.
///
/// Strict resolution fails on a missing variable, exactly as `serve` does. Lenient
/// resolution substitutes a harmless value of the right shape and records the name, so a
/// management command can validate the structure of a configuration whose unrelated
/// secrets are not present in its own environment.
pub struct Env<'a> {
    lookup: Lookup<'a>,
    lenient: bool,
    unresolved: Vec<String>,
}

impl<'a> Env<'a> {
    /// Fail when a referenced variable is unavailable.
    pub fn strict(lookup: impl FnMut(&str) -> Option<String> + 'a) -> Self {
        Self {
            lookup: Box::new(lookup),
            lenient: false,
            unresolved: Vec::new(),
        }
    }

    /// Substitute placeholders for unavailable variables and report them.
    pub fn lenient(lookup: impl FnMut(&str) -> Option<String> + 'a) -> Self {
        Self {
            lookup: Box::new(lookup),
            lenient: true,
            unresolved: Vec::new(),
        }
    }

    /// Variables that were referenced but unavailable (lenient mode only), sorted and unique.
    pub fn unresolved(&self) -> Vec<String> {
        let mut names = self.unresolved.clone();

        names.sort();
        names.dedup();

        names
    }

    fn get(&mut self, field: &str, variable: &str) -> Option<String> {
        if let Some(value) = (self.lookup)(variable) {
            return Some(value);
        }

        if !self.lenient {
            return None;
        }

        self.unresolved.push(variable.to_owned());

        Some(
            match field {
                "pluginHosts[].url" | "pluginHosts[].mediaOrigins[]" => {
                    "https://placeholder.invalid"
                }
                "listen" => "127.0.0.1:8787",
                "media.allowedFileRoots[]" => ".",
                "media.allowedUrlHosts[]" => "placeholder.invalid",
                _ => "placeholder-value-0123456789abcdef",
            }
            .to_owned(),
        )
    }
}

#[derive(Clone)]
pub struct AuthUser {
    pub username: String,
    pub password: String,
}

#[derive(Clone)]
pub struct Limits {
    pub max_connections: usize,
    pub max_client_sessions: usize,
    pub max_total_players: usize,
    pub max_players_per_client: usize,
    pub max_message_bytes: usize,
    pub request_timeout_ms: u64,
    pub max_in_flight_requests: usize,
    pub max_requests_per_second: usize,
    pub event_buffer: usize,
}

#[derive(Clone)]
pub struct MediaAccess {
    pub allowed_file_roots: Vec<PathBuf>,
    pub allowed_url_hosts: Vec<String>,
    pub allow_http: bool,
    pub block_private_networks: bool,
}

/// Validated crossfade policy. Native playback readiness is negotiated separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrossfadeConfig {
    pub enabled: bool,
    pub duration_ms: u32,
    pub prepare_before_ms: u32,
}

/// Opus encoder settings as written in configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpusSettings {
    pub bitrate: i32,
    pub complexity: i32,
    pub vbr: bool,
}

/// A fully validated configuration, independent of native libraries.
#[derive(Clone)]
pub struct Settings {
    pub bind_host: String,
    pub port: u16,
    pub users: Vec<AuthUser>,
    pub resume_timeout_seconds: u64,
    pub limits: Limits,
    pub media: MediaAccess,
    pub plugin_hosts: Vec<Arc<PluginHostConfig>>,
    pub opus: OpusSettings,
    pub crossfade: CrossfadeConfig,
    pub observability_console: bool,
}

/// Parse and validate a configuration document.
///
/// Relative media roots resolve against `base`.
pub fn parse(json: &str, base: &Path, env: &mut Env) -> Result<Settings, ConfigError> {
    let mut raw: RawConfig = serde_json::from_str(json)?;
    let opus = OpusSettings {
        bitrate: raw.audio.opus.bitrate,
        complexity: raw.audio.opus.complexity,
        vbr: raw.audio.opus.vbr,
    };

    if !(500..=512_000).contains(&opus.bitrate) {
        return Err(ConfigError::Invalid(
            "audio.opus.bitrate must be between 500 and 512000 bits per second",
        ));
    }

    if !(0..=10).contains(&opus.complexity) {
        return Err(ConfigError::Invalid(
            "audio.opus.complexity must be between 0 and 10",
        ));
    }

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
    let listen = resolve_env("listen", raw.listen, env)?;
    let address: std::net::SocketAddr = listen.parse().map_err(|_| {
        ConfigError::Invalid("listen must be an IP address and port, for example 127.0.0.1:8787")
    })?;
    let bind_host = address.ip().to_string();

    if let Some(auth) = raw.auth {
        if raw.users.is_some() {
            return Err(ConfigError::Invalid(
                "configure either auth or users, not both",
            ));
        }

        raw.users = Some(vec![raw::RawUser {
            username: auth.username,
            password: auth.password,
        }]);
    }

    let resume_timeout_seconds = raw.session_resumption_timeout_seconds;

    if resume_timeout_seconds == 0 || resume_timeout_seconds > 86_400 {
        return Err(ConfigError::Invalid(
            "sessionResumptionTimeoutSeconds must be between 1 and 86400",
        ));
    }

    let raw_users = raw.users.unwrap_or_default();
    let mut users = Vec::with_capacity(raw_users.len());

    for raw_user in raw_users {
        let username = resolve_env("users[].username", raw_user.username, env)?;
        let password = resolve_env("users[].password", raw_user.password, env)?;

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

    validate_limits(&limits)?;

    let mut allowed_file_roots = Vec::new();

    for configured_root in raw.media.allowed_file_roots {
        let configured_root = resolve_env("media.allowedFileRoots[]", configured_root, env)?;
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
        let host = resolve_env("media.allowedUrlHosts[]", host, env)?;
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

    let plugin_hosts = plugin_hosts::parse_plugin_hosts(raw.plugin_hosts, env)?;

    Ok(Settings {
        bind_host,
        port: address.port(),
        users,
        resume_timeout_seconds,
        limits,
        media: MediaAccess {
            allowed_file_roots,
            allowed_url_hosts,
            allow_http: raw.media.allow_http,
            block_private_networks: raw.media.block_private_networks,
        },
        plugin_hosts,
        opus,
        crossfade,
        observability_console: raw.observability.console,
    })
}

fn validate_limits(limits: &Limits) -> Result<(), ConfigError> {
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

    Ok(())
}

fn default_listen() -> String {
    "127.0.0.1:8787".to_owned()
}

fn default_username() -> String {
    "remedia".to_owned()
}

const fn default_resume_timeout() -> u64 {
    60
}

fn resolve_env(field: &str, value: String, env: &mut Env) -> Result<String, ConfigError> {
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

    env.get(field, variable)
        .ok_or_else(|| ConfigError::MissingEnvironment {
            field: field.to_owned(),
            variable: variable.to_owned(),
        })
}

/// Whether `name` is a valid `env('NAME')` variable name.
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();

    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
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

/// Whether an address is globally routable and therefore allowed without a private-network opt-in.
pub fn is_public_ip(address: IpAddr) -> bool {
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
