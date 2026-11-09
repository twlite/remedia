//! Pure parsing and validation for the Remedia v1 plugin-host HTTP contract.
use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

use super::error::{PluginError, PluginErrorKind};
use crate::config::PluginHostConfig;

pub(crate) const MANIFEST_PATH: &str = "/.well-known/remedia";
pub(crate) const EXECUTE_PATH: &str = "/remedia/v1/execute";
pub(crate) const MEDIA_TEMPLATE_PREFIX: &str = "/remedia/v1/media/";
const MAX_DESCRIPTOR_URL_BYTES: usize = 8_192;
const MAX_ERROR_BODY_BYTES: usize = 4_096;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawManifest {
    protocol_version: u64,
    #[serde(default)]
    host_id: Option<String>,
    endpoints: RawEndpoints,
    plugins: Vec<RawPlugin>,
}

#[derive(Deserialize)]
struct RawEndpoints {
    execute: String,
    media: String,
}

#[derive(Deserialize)]
struct RawPlugin {
    id: String,
    operations: Vec<String>,
}

/// A validated discovery document.
#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    advertised: HashSet<(String, String)>,
}

impl Manifest {
    pub(crate) fn advertises(&self, plugin_id: &str, operation: &str) -> bool {
        self.advertised
            .contains(&(plugin_id.to_owned(), operation.to_owned()))
    }

    pub(crate) fn operation_count(&self) -> usize {
        self.advertised.len()
    }
}

/// Parse and validate a discovery document against the deployment's configuration.
///
/// The advertised paths are verified rather than followed: the server only ever
/// contacts the protocol's fixed v1 routes beneath the configured origin.
pub(crate) fn validate_manifest(
    config: &PluginHostConfig,
    body: &[u8],
) -> Result<Manifest, PluginError> {
    let manifest: RawManifest = serde_json::from_slice(body).map_err(|_| {
        PluginError::new(
            PluginErrorKind::Incompatible,
            "plugin host discovery response is not a Remedia manifest",
        )
    })?;

    if manifest.protocol_version != 1 {
        return Err(PluginError::new(
            PluginErrorKind::Incompatible,
            "plugin host uses an unsupported protocol version",
        ));
    }

    // A deployment identifier is an optional extension, but it must match when present.
    if manifest
        .host_id
        .as_deref()
        .is_some_and(|host_id| host_id != config.id)
    {
        return Err(PluginError::new(
            PluginErrorKind::Incompatible,
            "plugin host identity does not match the configured host ID",
        ));
    }

    if manifest.endpoints.execute != EXECUTE_PATH
        || !manifest.endpoints.media.starts_with(MEDIA_TEMPLATE_PREFIX)
    {
        return Err(PluginError::new(
            PluginErrorKind::Incompatible,
            "plugin host advertises unsupported v1 routes",
        ));
    }

    let mut advertised = HashSet::new();

    for plugin in manifest.plugins {
        for operation in plugin.operations {
            advertised.insert((plugin.id.clone(), operation));
        }
    }

    Ok(Manifest { advertised })
}

/// Result of a successful execute call.
#[derive(Debug)]
pub(crate) enum ExecuteOutcome {
    Declined,
    Handled(Value),
}

pub(crate) fn parse_execute_response(body: &[u8]) -> Result<ExecuteOutcome, PluginError> {
    let invalid = || {
        PluginError::new(
            PluginErrorKind::InvalidResponse,
            "plugin host returned an invalid execute response",
        )
    };
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let object = value.as_object().ok_or_else(invalid)?;

    match object.get("handled").and_then(Value::as_bool) {
        Some(false) => Ok(ExecuteOutcome::Declined),
        Some(true) => object
            .get("result")
            .cloned()
            .map(ExecuteOutcome::Handled)
            .ok_or_else(invalid),
        None => Err(invalid()),
    }
}

/// A media location returned by an operation.
#[derive(Debug)]
pub(crate) struct Descriptor {
    pub(crate) url: url::Url,
    pub(crate) expires_at_ms: Option<u64>,
}

pub(crate) fn parse_descriptor(value: &Value) -> Result<Descriptor, PluginError> {
    let invalid = |detail| PluginError::new(PluginErrorKind::MediaInvalid, detail);
    let object = value
        .as_object()
        .ok_or_else(|| invalid("operation result is not a media descriptor"))?;
    let raw = object
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= MAX_DESCRIPTOR_URL_BYTES)
        .ok_or_else(|| invalid("media descriptor has no usable url"))?;
    let url = url::Url::parse(raw).map_err(|_| invalid("media descriptor url is invalid"))?;

    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "media descriptor url must be an HTTP(S) URL without credentials or fragment",
        ));
    }

    let expires_at_ms = match object.get("expiresAtMs") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| invalid("media descriptor expiresAtMs is invalid"))?,
        ),
    };

    Ok(Descriptor { url, expires_at_ms })
}

/// Map a non-success execute status and optional protocol error code to a failure.
pub(crate) fn error_from_status(status: u16, code: Option<&str>) -> PluginError {
    use PluginErrorKind as Kind;

    let (kind, detail) = match (status, code) {
        (401 | 403, _) => (
            Kind::Unauthorized,
            "plugin host rejected the configured credentials",
        ),
        (404, Some("plugin_not_found" | "operation_not_found")) => (
            Kind::OperationUnavailable,
            "plugin host does not provide the requested operation",
        ),
        (404 | 405, _) => (
            Kind::Incompatible,
            "plugin host does not implement the Remedia v1 execute route",
        ),
        (499, _) => (Kind::Cancelled, "plugin host cancelled the operation"),
        (400 | 413 | 415 | 422, _) => (
            Kind::InvalidRequest,
            "plugin host rejected the operation request",
        ),
        (502, Some("plugin_failed" | "invalid_result" | "invalid_media_source")) => {
            (Kind::HostFailed, "plugin operation failed")
        }
        (300..=399, _) => (
            Kind::Incompatible,
            "plugin host attempted a redirect, which is never followed",
        ),
        (408 | 425 | 429 | 500..=599, _) => (
            Kind::HostUnavailable,
            "plugin host is temporarily unavailable",
        ),
        _ => (
            Kind::HostFailed,
            "plugin host returned an unexpected response",
        ),
    };

    PluginError::new(kind, detail)
}

/// Extract the stable `error.code` from a bounded error body.
pub(crate) fn error_code(body: &[u8]) -> Option<String> {
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }

    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("error")?
        .get("code")?
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config() -> PluginHostConfig {
        let json = r#"{"users":[{"username":"u","password":"p"}],"pluginHosts":[{"id":"host-a","endpoint":"https://example.com","operations":[{"pluginId":"p","operation":"o"}]}]}"#;

        crate::config::ServerConfig::from_json_for_tests(json)
            .plugin_hosts
            .remove(0)
            .as_ref()
            .clone()
    }

    fn manifest(overrides: Value) -> Vec<u8> {
        let mut value = json!({
            "name": "Host",
            "protocolVersion": 1,
            "endpoints": {"execute": EXECUTE_PATH, "media": "/remedia/v1/media/{token}"},
            "plugins": [{"id": "p", "operations": ["o"]}]
        });

        if let (Some(target), Some(patch)) = (value.as_object_mut(), overrides.as_object()) {
            target.extend(patch.clone());
        }

        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn accepts_a_v1_manifest_and_reports_advertised_operations() {
        let manifest = validate_manifest(&config(), &manifest(json!({}))).unwrap();

        assert!(manifest.advertises("p", "o"));
        assert!(!manifest.advertises("p", "other"));
        assert_eq!(manifest.operation_count(), 1);
    }

    #[test]
    fn rejects_incompatible_versions_routes_and_identities() {
        for overrides in [
            json!({"protocolVersion": 2}),
            json!({"endpoints": {"execute": "/other", "media": "/remedia/v1/media/{token}"}}),
            json!({"endpoints": {"execute": EXECUTE_PATH, "media": "https://evil.example/{token}"}}),
            json!({"hostId": "host-b"}),
        ] {
            let error = validate_manifest(&config(), &manifest(overrides)).unwrap_err();

            assert_eq!(error.kind, PluginErrorKind::Incompatible);
        }

        assert!(validate_manifest(&config(), &manifest(json!({"hostId": "host-a"}))).is_ok());
        assert!(validate_manifest(&config(), b"not json").is_err());
    }

    #[test]
    fn execute_responses_distinguish_declined_handled_and_malformed() {
        assert!(matches!(
            parse_execute_response(br#"{"handled":false}"#).unwrap(),
            ExecuteOutcome::Declined
        ));
        assert!(matches!(
            parse_execute_response(br#"{"handled":true,"result":null}"#).unwrap(),
            ExecuteOutcome::Handled(Value::Null)
        ));
        assert!(parse_execute_response(br#"{"handled":true}"#).is_err());
        assert!(parse_execute_response(br#"[]"#).is_err());
    }

    #[test]
    fn descriptors_require_safe_urls_and_integer_expirations() {
        let descriptor =
            parse_descriptor(&json!({"url": "https://media.example/a", "expiresAtMs": 12}))
                .unwrap();

        assert_eq!(descriptor.expires_at_ms, Some(12));

        for value in [
            json!({"url": "file:///etc/passwd"}),
            json!({"url": "https://user:pass@media.example/a"}),
            json!({"url": "https://media.example/a#fragment"}),
            json!({"url": "https://media.example/a", "expiresAtMs": "soon"}),
            json!({"url": ""}),
            json!({"url": 12}),
            json!("https://media.example/a"),
        ] {
            assert_eq!(
                parse_descriptor(&value).unwrap_err().kind,
                PluginErrorKind::MediaInvalid
            );
        }
    }

    #[test]
    fn status_mapping_separates_authentication_policy_and_availability() {
        assert_eq!(
            error_from_status(401, None).kind,
            PluginErrorKind::Unauthorized
        );
        assert_eq!(
            error_from_status(404, Some("operation_not_found")).kind,
            PluginErrorKind::OperationUnavailable
        );
        assert_eq!(
            error_from_status(404, None).kind,
            PluginErrorKind::Incompatible
        );
        assert_eq!(
            error_from_status(503, None).kind,
            PluginErrorKind::HostUnavailable
        );
        assert_eq!(
            error_from_status(502, Some("plugin_failed")).kind,
            PluginErrorKind::HostFailed
        );
        assert_eq!(
            error_from_status(307, None).kind,
            PluginErrorKind::Incompatible
        );
        assert!(!error_from_status(401, None).retriable());
        assert!(error_from_status(503, None).retriable());
        assert_eq!(
            error_code(br#"{"error":{"code":"unauthorized"}}"#).as_deref(),
            Some("unauthorized")
        );
    }
}
