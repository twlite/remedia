//! Pure parsing and validation for the Remedia v1 plugin-host HTTP contract.
use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

use crate::PluginHostConfig;
use crate::error::{PluginError, PluginErrorKind};

pub const MANIFEST_PATH: &str = "/.well-known/remedia";
pub const EXECUTE_PATH: &str = "/remedia/v1/execute";
pub const MEDIA_TEMPLATE_PREFIX: &str = "/remedia/v1/media/";
const MAX_DESCRIPTOR_URL_BYTES: usize = 8_192;
const MAX_ERROR_BODY_BYTES: usize = 4_096;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawManifest {
    protocol_version: u64,
    #[serde(default)]
    name: Option<String>,
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
pub struct Manifest {
    advertised: HashSet<(String, String)>,
    name: Option<String>,
    host_id: Option<String>,
}

impl Manifest {
    pub fn advertises(&self, plugin_id: &str, operation: &str) -> bool {
        self.advertised
            .contains(&(plugin_id.to_owned(), operation.to_owned()))
    }

    pub fn operation_count(&self) -> usize {
        self.advertised.len()
    }

    /// Advertised `(pluginId, operation)` pairs, sorted for stable output.
    pub fn operations(&self) -> Vec<(String, String)> {
        let mut operations: Vec<_> = self.advertised.iter().cloned().collect();

        operations.sort();

        operations
    }

    /// The host's display name. Never an identity: use `host_id` or an operator-chosen ID.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The deployment identifier declared by the host, when it declares one.
    pub fn host_id(&self) -> Option<&str> {
        self.host_id.as_deref()
    }
}

/// Parse and validate a discovery document against the deployment's configuration.
///
/// The advertised paths are verified rather than followed: the server only ever
/// contacts the protocol's fixed v1 routes beneath the configured origin.
pub fn validate_manifest(config: &PluginHostConfig, body: &[u8]) -> Result<Manifest, PluginError> {
    parse_manifest(body, Some(&config.id))
}

/// Parse and validate a discovery document.
///
/// `expected_id` is the configured host ID when one exists. Pass `None` while a host is
/// still being registered: the document is then validated without an identity check, and
/// the caller must ensure any declared `hostId` becomes the configured ID.
pub fn parse_manifest(body: &[u8], expected_id: Option<&str>) -> Result<Manifest, PluginError> {
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
        .zip(expected_id)
        .is_some_and(|(declared, expected)| declared != expected)
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

    let name = manifest.name;
    let host_id = manifest.host_id;

    for plugin in manifest.plugins {
        for operation in plugin.operations {
            advertised.insert((plugin.id.clone(), operation));
        }
    }

    Ok(Manifest {
        advertised,
        name,
        host_id,
    })
}

/// Result of a successful execute call.
#[derive(Debug)]
pub enum ExecuteOutcome {
    Declined,
    Handled(Value),
}

pub fn parse_execute_response(body: &[u8]) -> Result<ExecuteOutcome, PluginError> {
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
pub struct Descriptor {
    pub url: url::Url,
    pub expires_at_ms: Option<u64>,
}

pub fn parse_descriptor(value: &Value) -> Result<Descriptor, PluginError> {
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
pub fn error_from_status(status: u16, code: Option<&str>) -> PluginError {
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
pub fn error_code(body: &[u8]) -> Option<String> {
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

        crate::parse(
            json,
            std::path::Path::new("."),
            &mut crate::Env::strict(|_| None),
        )
        .expect("test configuration is valid")
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
