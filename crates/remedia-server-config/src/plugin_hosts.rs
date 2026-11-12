//! Operator-owned registrations for generic Remedia plugin hosts.

use super::*;

const MAX_PLUGIN_HOSTS: usize = 64;
const MAX_OPERATIONS_PER_HOST: usize = 256;
const MAX_TOKEN_BYTES: usize = 4096;
const MIN_TOKEN_BYTES: usize = 16;

/// A bearer secret which never appears in debug output or error messages.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// One operation a deployment is permitted to execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowedOperation {
    pub plugin_id: String,
    pub operation: String,
    /// Whether executing the operation again is free of side effects. Only
    /// replay-safe operations are retried or refreshed automatically.
    pub replay_safe: bool,
}

#[derive(Clone, Debug)]
pub struct PluginHostLimits {
    pub request_timeout: Duration,
    pub discovery_ttl: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_pending_operations: usize,
    pub max_active_streams: usize,
    pub max_media_bytes: u64,
    pub media_idle_timeout: Duration,
    pub media_buffer_bytes: usize,
    pub max_attempts: u32,
}

/// A validated plugin-host deployment.
#[derive(Clone, Debug)]
pub struct PluginHostConfig {
    pub id: String,
    /// Disabled hosts are validated and retained but never registered as executable.
    pub enabled: bool,
    /// Origin of the execution and discovery URL.
    pub url: url::Url,
    pub token: Option<Secret>,
    pub allow_http: bool,
    /// Origins, including the configured URL origin, whose media is fetched by the server
    /// without credentials.
    pub media_origins: Vec<String>,
    pub allow_external_media: bool,
    pub operations: Vec<AllowedOperation>,
    pub limits: PluginHostLimits,
}

impl PluginHostConfig {
    pub fn operation(&self, plugin_id: &str, operation: &str) -> Option<&AllowedOperation> {
        self.operations
            .iter()
            .find(|allowed| allowed.plugin_id == plugin_id && allowed.operation == operation)
    }

    pub fn owns_media_origin(&self, url: &url::Url) -> bool {
        self.media_origins
            .iter()
            .any(|origin| *origin == url.origin().ascii_serialization())
    }
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawPluginHost {
    id: String,
    #[serde(default = "default_enabled")]
    enabled: bool,
    url: String,
    #[serde(default)]
    auth: RawPluginAuth,
    #[serde(default)]
    allow_http: bool,
    #[serde(default)]
    media_origins: Vec<String>,
    #[serde(default)]
    allow_external_media: bool,
    operations: Vec<RawAllowedOperation>,
    #[serde(default)]
    limits: RawPluginHostLimits,
}

const fn default_enabled() -> bool {
    true
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Default, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
enum RawPluginAuth {
    #[default]
    None,
    Bearer {
        token: String,
    },
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize)]
#[serde(untagged)]
enum RawAllowedOperation {
    Name(String),
    Details(OperationDetails),
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OperationDetails {
    plugin_id: String,
    operation: String,
    #[serde(default)]
    replay_safe: bool,
}

#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawPluginHostLimits {
    request_timeout_ms: u64,
    discovery_ttl_ms: u64,
    max_request_bytes: usize,
    max_response_bytes: usize,
    max_pending_operations: usize,
    max_active_streams: usize,
    max_media_bytes: u64,
    media_idle_timeout_ms: u64,
    media_buffer_bytes: usize,
    max_attempts: u32,
}

impl Default for RawPluginHostLimits {
    fn default() -> Self {
        Self {
            request_timeout_ms: 10_000,
            discovery_ttl_ms: 60_000,
            max_request_bytes: 65_536,
            max_response_bytes: 65_536,
            max_pending_operations: 8,
            max_active_streams: 8,
            max_media_bytes: 1_073_741_824,
            media_idle_timeout_ms: 15_000,
            media_buffer_bytes: 262_144,
            max_attempts: 2,
        }
    }
}

/// Plugin and operation names use the same grammar as `@remedia/plugin-host`.
pub fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();

    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Parse an origin-only HTTP(S) URL: no credentials, path, query or fragment.
pub fn parse_origin(value: &str, allow_http: bool) -> Result<url::Url, ConfigError> {
    let url = url::Url::parse(value.trim())
        .map_err(|_| ConfigError::Invalid("pluginHosts URLs must be absolute HTTP(S) origins"))?;

    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(ConfigError::Invalid(
            "pluginHosts URLs must be HTTP(S) origins without credentials, path, query or fragment",
        ));
    }

    if url.scheme() == "http" && !allow_http {
        return Err(ConfigError::Invalid(
            "pluginHosts endpoints must use https unless allowHttp is true",
        ));
    }

    Ok(url)
}

fn ranged<T: PartialOrd + Copy>(
    value: T,
    low: T,
    high: T,
    message: &'static str,
) -> Result<T, ConfigError> {
    if value < low || value > high {
        return Err(ConfigError::Invalid(message));
    }

    Ok(value)
}

pub(super) fn parse_plugin_hosts(
    raw: Vec<RawPluginHost>,
    env: &mut Env,
) -> Result<Vec<Arc<PluginHostConfig>>, ConfigError> {
    if raw.len() > MAX_PLUGIN_HOSTS {
        return Err(ConfigError::Invalid(
            "pluginHosts contains too many entries",
        ));
    }

    let mut hosts: Vec<Arc<PluginHostConfig>> = Vec::with_capacity(raw.len());

    for host in raw {
        if !valid_identifier(&host.id) {
            return Err(ConfigError::Invalid(
                "pluginHosts[].id must be 1-128 letters, digits, dots, underscores or hyphens and start alphanumeric",
            ));
        }

        if hosts.iter().any(|existing| existing.id == host.id) {
            return Err(ConfigError::Invalid(
                "pluginHosts[].id values must be unique",
            ));
        }

        let url = resolve_env("pluginHosts[].url", host.url, env)?;
        let url = parse_origin(&url, host.allow_http)?;
        let token = match host.auth {
            RawPluginAuth::None => None,
            RawPluginAuth::Bearer { token } => {
                let token = resolve_env("pluginHosts[].auth.token", token, env)?;

                if token.len() < MIN_TOKEN_BYTES
                    || token.len() > MAX_TOKEN_BYTES
                    || token
                        .bytes()
                        .any(|byte| byte.is_ascii_control() || byte == b' ')
                {
                    return Err(ConfigError::Invalid(
                        "pluginHosts[].auth.token must be 16 to 4096 bytes without spaces or control characters",
                    ));
                }

                Some(Secret(token))
            }
        };
        let mut media_origins = vec![url.origin().ascii_serialization()];

        for origin in host.media_origins {
            let origin = resolve_env("pluginHosts[].mediaOrigins[]", origin, env)?;
            let origin = parse_origin(&origin, host.allow_http)?
                .origin()
                .ascii_serialization();

            if !media_origins.contains(&origin) {
                media_origins.push(origin);
            }
        }

        if host.operations.is_empty() || host.operations.len() > MAX_OPERATIONS_PER_HOST {
            return Err(ConfigError::Invalid(
                "pluginHosts[].operations must contain 1 to 256 allowed operations",
            ));
        }

        let mut operations: Vec<AllowedOperation> = Vec::with_capacity(host.operations.len());

        for operation in host.operations {
            let operation = match operation {
                RawAllowedOperation::Details(details) => details,
                RawAllowedOperation::Name(name) => {
                    let (plugin, operation) = name.split_once('/').ok_or(ConfigError::Invalid(
                        "pluginHosts operations must use PLUGIN/OPERATION",
                    ))?;

                    OperationDetails {
                        plugin_id: plugin.into(),
                        operation: operation.into(),
                        replay_safe: false,
                    }
                }
            };
            if !valid_identifier(&operation.plugin_id) || !valid_identifier(&operation.operation) {
                return Err(ConfigError::Invalid(
                    "pluginHosts[].operations[] pluginId and operation must be valid identifiers",
                ));
            }

            if operations.iter().any(|existing| {
                existing.plugin_id == operation.plugin_id
                    && existing.operation == operation.operation
            }) {
                return Err(ConfigError::Invalid(
                    "pluginHosts[].operations[] entries must be unique",
                ));
            }

            operations.push(AllowedOperation {
                plugin_id: operation.plugin_id,
                operation: operation.operation,
                replay_safe: operation.replay_safe,
            });
        }

        let raw_limits = host.limits;
        let request_timeout_ms = ranged(
            raw_limits.request_timeout_ms,
            1,
            300_000,
            "pluginHosts[].limits.requestTimeoutMs must be between 1 and 300000",
        )?;
        let discovery_ttl_ms = ranged(
            raw_limits.discovery_ttl_ms,
            1_000,
            3_600_000,
            "pluginHosts[].limits.discoveryTtlMs must be between 1000 and 3600000",
        )?;
        let media_idle_timeout_ms = ranged(
            raw_limits.media_idle_timeout_ms,
            1,
            300_000,
            "pluginHosts[].limits.mediaIdleTimeoutMs must be between 1 and 300000",
        )?;
        let limits = PluginHostLimits {
            request_timeout: Duration::from_millis(request_timeout_ms),
            discovery_ttl: Duration::from_millis(discovery_ttl_ms),
            max_request_bytes: ranged(
                raw_limits.max_request_bytes,
                64,
                16_777_216,
                "pluginHosts[].limits.maxRequestBytes must be between 64 and 16777216",
            )?,
            max_response_bytes: ranged(
                raw_limits.max_response_bytes,
                256,
                16_777_216,
                "pluginHosts[].limits.maxResponseBytes must be between 256 and 16777216",
            )?,
            max_pending_operations: ranged(
                raw_limits.max_pending_operations,
                1,
                1_024,
                "pluginHosts[].limits.maxPendingOperations must be between 1 and 1024",
            )?,
            max_active_streams: ranged(
                raw_limits.max_active_streams,
                1,
                1_024,
                "pluginHosts[].limits.maxActiveStreams must be between 1 and 1024",
            )?,
            max_media_bytes: ranged(
                raw_limits.max_media_bytes,
                1_024,
                68_719_476_736,
                "pluginHosts[].limits.maxMediaBytes must be between 1024 and 68719476736",
            )?,
            media_idle_timeout: Duration::from_millis(media_idle_timeout_ms),
            media_buffer_bytes: ranged(
                raw_limits.media_buffer_bytes,
                4_096,
                16_777_216,
                "pluginHosts[].limits.mediaBufferBytes must be between 4096 and 16777216",
            )?,
            max_attempts: ranged(
                raw_limits.max_attempts,
                1,
                3,
                "pluginHosts[].limits.maxAttempts must be between 1 and 3",
            )?,
        };

        hosts.push(Arc::new(PluginHostConfig {
            id: host.id,
            enabled: host.enabled,
            url,
            token,
            allow_http: host.allow_http,
            media_origins,
            allow_external_media: host.allow_external_media,
            operations,
            limits,
        }));
    }

    Ok(hosts)
}
