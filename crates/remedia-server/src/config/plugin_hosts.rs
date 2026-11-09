//! Operator-owned registrations for generic Remedia plugin hosts.
use std::net::IpAddr;

use super::*;

const MAX_PLUGIN_HOSTS: usize = 64;
const MAX_OPERATIONS_PER_HOST: usize = 256;
const MAX_TOKEN_BYTES: usize = 4096;
const MIN_TOKEN_BYTES: usize = 16;

/// A bearer secret which never appears in debug output or error messages.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn expose(&self) -> &str {
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
pub(crate) struct AllowedOperation {
    pub(crate) plugin_id: String,
    pub(crate) operation: String,
    /// Whether executing the operation again is free of side effects. Only
    /// replay-safe operations are retried or refreshed automatically.
    pub(crate) replay_safe: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginHostLimits {
    pub(crate) request_timeout: Duration,
    pub(crate) discovery_ttl: Duration,
    pub(crate) max_request_bytes: usize,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_pending_operations: usize,
    pub(crate) max_active_streams: usize,
    pub(crate) max_media_bytes: u64,
    pub(crate) media_idle_timeout: Duration,
    pub(crate) media_buffer_bytes: usize,
    pub(crate) max_attempts: u32,
}

/// A validated plugin-host deployment.
#[derive(Clone, Debug)]
pub(crate) struct PluginHostConfig {
    pub(crate) id: String,
    /// Origin of the execution and discovery endpoint.
    pub(crate) endpoint: url::Url,
    pub(crate) token: Option<Secret>,
    pub(crate) allow_insecure_http: bool,
    pub(crate) allow_private_network: bool,
    /// Origins, including the endpoint origin, whose media is fetched by the server
    /// without credentials.
    pub(crate) media_origins: Vec<String>,
    pub(crate) allow_external_media: bool,
    pub(crate) operations: Vec<AllowedOperation>,
    pub(crate) limits: PluginHostLimits,
}

impl PluginHostConfig {
    pub(crate) fn operation(&self, plugin_id: &str, operation: &str) -> Option<&AllowedOperation> {
        self.operations
            .iter()
            .find(|allowed| allowed.plugin_id == plugin_id && allowed.operation == operation)
    }

    pub(crate) fn owns_media_origin(&self, url: &url::Url) -> bool {
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
    endpoint: String,
    #[serde(default)]
    auth: RawPluginAuth,
    #[serde(default)]
    allow_insecure_http: bool,
    #[serde(default)]
    allow_private_network: bool,
    #[serde(default)]
    media_origins: Vec<String>,
    #[serde(default)]
    allow_external_media: bool,
    operations: Vec<RawAllowedOperation>,
    #[serde(default)]
    limits: RawPluginHostLimits,
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
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawAllowedOperation {
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
pub(crate) fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();

    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Parse an origin-only HTTP(S) URL: no credentials, path, query or fragment.
pub(crate) fn parse_origin(
    value: &str,
    allow_insecure_http: bool,
) -> Result<url::Url, ConfigError> {
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

    if url.scheme() == "http" && !allow_insecure_http {
        return Err(ConfigError::Invalid(
            "pluginHosts endpoints must use https unless allowInsecureHttp is true",
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
    get_env: &mut impl FnMut(&str) -> Option<String>,
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

        let endpoint = resolve_env("pluginHosts[].endpoint", host.endpoint, get_env)?;
        let endpoint = parse_origin(&endpoint, host.allow_insecure_http)?;
        let token = match host.auth {
            RawPluginAuth::None => None,
            RawPluginAuth::Bearer { token } => {
                let token = resolve_env("pluginHosts[].auth.token", token, get_env)?;

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
        if let Some(address) = match endpoint.host() {
            Some(url::Host::Ipv4(address)) => Some(IpAddr::V4(address)),
            Some(url::Host::Ipv6(address)) => Some(IpAddr::V6(address)),
            _ => None,
        } && !host.allow_private_network
            && !is_public_ip(address)
        {
            return Err(ConfigError::Invalid(
                "pluginHosts endpoints with private or loopback addresses require allowPrivateNetwork",
            ));
        }

        let mut media_origins = vec![endpoint.origin().ascii_serialization()];

        for origin in host.media_origins {
            let origin = resolve_env("pluginHosts[].mediaOrigins[]", origin, get_env)?;
            let origin = parse_origin(&origin, host.allow_insecure_http)?
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
            endpoint,
            token,
            allow_insecure_http: host.allow_insecure_http,
            allow_private_network: host.allow_private_network,
            media_origins,
            allow_external_media: host.allow_external_media,
            operations,
            limits,
        }));
    }

    Ok(hosts)
}
