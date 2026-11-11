//! Generic Remedia plugin-host integration: discovery, authenticated operation execution,
//! and conversion of operation results into decoder inputs.
//!
//! Remedia treats operation input as opaque JSON and never interprets application-specific
//! provider references. A host is addressed only by its operator-assigned ID.
//!
//! Two consumers share one execution path (discovery, allowlist, concurrency, retries, health):
//! generic forwarding returns the operation's JSON verbatim, and media resolution additionally
//! interprets the result as a media descriptor and opens the encoded stream.
mod media;
mod registry;

pub(crate) use media::MediaGuard;
pub use registry::HostDiagnostic;
pub(crate) use registry::{HostListing, OperationRef, PluginHostRegistry};
pub(crate) use remedia_server_config::error::{PluginError, PluginErrorKind};
pub(crate) use remedia_server_config::protocol::ExecuteOutcome;

use crate::config::ServerConfig;

/// Discover every configured plugin host once and report its health.
///
/// This performs real network requests to the configured endpoints and is intended for
/// diagnostics such as `remedia-server doctor`. It does not start a server.
pub async fn diagnose_plugin_hosts(config: &ServerConfig) -> Vec<HostDiagnostic> {
    let Ok(registry) = PluginHostRegistry::new(
        &config.plugin_hosts,
        &remedia_observability::ObservabilityHandle::default(),
    ) else {
        return Vec::new();
    };
    let mut diagnostics = registry.diagnose().await;

    // Disabled hosts are not contacted, but doctor still lists them so they are not forgotten.
    diagnostics.extend(
        config
            .plugin_hosts
            .iter()
            .filter(|host| !host.enabled)
            .map(|host| HostDiagnostic {
                id: host.id.clone(),
                origin: host.endpoint.origin().ascii_serialization(),
                state: "disabled",
                detail: None,
                advertised_operations: 0,
                missing_operations: Vec::new(),
            }),
    );

    registry.close().await;

    diagnostics
}
