use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use remedia_core::OpusEncoderConfig;
use remedia_observability::{ObservabilityConfig, ObservabilityError};

pub(crate) use remedia_server_config::{
    AuthUser, CrossfadeConfig, CrossfadeCurve, Limits, MediaAccess, PluginHostConfig, is_public_ip,
    valid_identifier,
};

/// The native engine's curve for a configured or requested curve.
pub(crate) const fn engine_curve(curve: CrossfadeCurve) -> remedia_core::CrossfadeCurve {
    match curve {
        CrossfadeCurve::EqualPower => remedia_core::CrossfadeCurve::EqualPower,
        CrossfadeCurve::Adaptive => remedia_core::CrossfadeCurve::Adaptive,
    }
}

#[derive(Clone)]
pub struct ServerConfig {
    pub(crate) bind_host: String,
    pub(crate) port: u16,
    pub(crate) users: Vec<AuthUser>,
    pub(crate) resume_timeout_seconds: u64,
    pub(crate) limits: Limits,
    pub(crate) media: MediaAccess,
    pub(crate) plugin_hosts: Vec<Arc<PluginHostConfig>>,
    pub(crate) opus: OpusEncoderConfig,
    pub(crate) crossfade: CrossfadeConfig,
    pub(crate) observability_console: bool,
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

impl From<remedia_server_config::ConfigError> for ConfigError {
    fn from(error: remedia_server_config::ConfigError) -> Self {
        use remedia_server_config::ConfigError as Shared;

        match error {
            Shared::MediaRootIo { path, source } => Self::MediaRootIo { path, source },
            Shared::Json(error) => Self::Json(error),
            Shared::Invalid(message) => Self::Invalid(message),
            Shared::MissingEnvironment { field, variable } => {
                Self::MissingEnvironment { field, variable }
            }
            Shared::InvalidEnvironmentReference(field) => Self::InvalidEnvironmentReference(field),
        }
    }
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
        get_env: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let shared = remedia_server_config::parse(
            json,
            base,
            &mut remedia_server_config::Env::strict(get_env),
        )?;
        // The shared crate validates ranges without native libraries; the encoder
        // configuration is the authority for what the native runtime accepts.
        let opus = OpusEncoderConfig {
            bitrate: shared.opus.bitrate,
            complexity: shared.opus.complexity,
            vbr: shared.opus.vbr,
        };

        opus.validate().map_err(ConfigError::Audio)?;

        Ok(Self {
            bind_host: shared.bind_host,
            port: shared.port,
            users: shared.users,
            resume_timeout_seconds: shared.resume_timeout_seconds,
            limits: shared.limits,
            media: shared.media,
            plugin_hosts: shared.plugin_hosts,
            observability_console: shared.observability_console,
            opus,
            crossfade: shared.crossfade,
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

pub(crate) fn parse_snowflake(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
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

#[cfg(test)]
mod tests;

mod media_access;
