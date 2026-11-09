//! Sanitized plugin-host failures. Messages never contain URLs, credentials or operation input.
use std::fmt;

/// A low-cardinality failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginErrorKind {
    /// The host ID is not configured on this node.
    HostNotFound,
    /// The operation is not in the host's configured allowlist.
    OperationNotAllowed,
    /// The host does not currently advertise the allowed operation.
    OperationUnavailable,
    /// The host could not be reached or reported a transient failure.
    HostUnavailable,
    /// The host rejected the configured credentials.
    Unauthorized,
    /// The host reported a plugin or protocol failure.
    HostFailed,
    /// The host rejected the request as malformed.
    InvalidRequest,
    /// The operation input exceeds the configured request limit.
    RequestTooLarge,
    /// The operation returned no result.
    Declined,
    /// The host response did not match the v1 protocol.
    InvalidResponse,
    /// The host response exceeded the configured limit.
    ResponseTooLarge,
    /// The discovery document is incompatible with this node or configuration.
    Incompatible,
    /// The operation result is not a usable media descriptor.
    MediaInvalid,
    /// The descriptor refers to an origin this deployment may not fetch from.
    MediaOriginNotAllowed,
    /// The media grant expired or was already redeemed.
    MediaExpired,
    /// The media transfer failed.
    MediaFailed,
    /// A configured pending-operation or active-stream limit was reached.
    Exhausted,
    /// A configured deadline elapsed.
    Timeout,
    /// The enclosing playback generation or preparation was cancelled.
    Cancelled,
    /// The node is shutting down.
    ShuttingDown,
}

/// A plugin-host failure with a stable machine code and a static, safe description.
#[derive(Clone, Debug)]
pub(crate) struct PluginError {
    pub(crate) kind: PluginErrorKind,
    pub(crate) detail: &'static str,
}

impl PluginError {
    pub(crate) const fn new(kind: PluginErrorKind, detail: &'static str) -> Self {
        Self { kind, detail }
    }

    /// The RPC error code exposed to clients.
    pub(crate) const fn rpc_code(&self) -> &'static str {
        match self.kind {
            PluginErrorKind::HostNotFound => "plugin_host_not_found",
            PluginErrorKind::OperationNotAllowed => "plugin_operation_not_allowed",
            PluginErrorKind::OperationUnavailable => "plugin_operation_unavailable",
            PluginErrorKind::HostUnavailable => "plugin_host_unavailable",
            PluginErrorKind::Unauthorized => "plugin_host_unauthorized",
            PluginErrorKind::Incompatible => "plugin_host_incompatible",
            PluginErrorKind::HostFailed
            | PluginErrorKind::InvalidResponse
            | PluginErrorKind::ResponseTooLarge => "plugin_operation_failed",
            PluginErrorKind::InvalidRequest | PluginErrorKind::RequestTooLarge => "invalid_request",
            PluginErrorKind::Declined => "plugin_operation_declined",
            PluginErrorKind::MediaInvalid | PluginErrorKind::MediaOriginNotAllowed => {
                "plugin_media_invalid"
            }
            PluginErrorKind::MediaExpired => "plugin_media_expired",
            PluginErrorKind::MediaFailed => "plugin_media_failed",
            PluginErrorKind::Exhausted => "resource_exhausted",
            PluginErrorKind::Timeout => "request_timeout",
            PluginErrorKind::Cancelled => "cancelled",
            PluginErrorKind::ShuttingDown => "invalid_state",
        }
    }

    /// Stable label for diagnostics and statistics.
    pub(crate) const fn label(&self) -> &'static str {
        match self.kind {
            PluginErrorKind::HostNotFound => "host_not_found",
            PluginErrorKind::OperationNotAllowed => "operation_not_allowed",
            PluginErrorKind::OperationUnavailable => "operation_unavailable",
            PluginErrorKind::HostUnavailable => "host_unavailable",
            PluginErrorKind::Unauthorized => "unauthorized",
            PluginErrorKind::HostFailed => "host_failed",
            PluginErrorKind::InvalidRequest => "invalid_request",
            PluginErrorKind::RequestTooLarge => "request_too_large",
            PluginErrorKind::Declined => "declined",
            PluginErrorKind::InvalidResponse => "invalid_response",
            PluginErrorKind::ResponseTooLarge => "response_too_large",
            PluginErrorKind::Incompatible => "incompatible",
            PluginErrorKind::MediaInvalid => "media_invalid",
            PluginErrorKind::MediaOriginNotAllowed => "media_origin_not_allowed",
            PluginErrorKind::MediaExpired => "media_expired",
            PluginErrorKind::MediaFailed => "media_failed",
            PluginErrorKind::Exhausted => "exhausted",
            PluginErrorKind::Timeout => "timeout",
            PluginErrorKind::Cancelled => "cancelled",
            PluginErrorKind::ShuttingDown => "shutting_down",
        }
    }

    /// Whether the failure reflects the host's health rather than the caller's request.
    pub(crate) const fn affects_health(&self) -> bool {
        matches!(
            self.kind,
            PluginErrorKind::HostUnavailable
                | PluginErrorKind::Unauthorized
                | PluginErrorKind::HostFailed
                | PluginErrorKind::InvalidResponse
                | PluginErrorKind::ResponseTooLarge
                | PluginErrorKind::Incompatible
                | PluginErrorKind::Timeout
        )
    }

    /// Whether repeating a replay-safe operation can plausibly succeed. Authentication,
    /// authorization-policy and validation failures are never retried.
    pub(crate) const fn retriable(&self) -> bool {
        matches!(
            self.kind,
            PluginErrorKind::HostUnavailable | PluginErrorKind::Timeout
        )
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for PluginError {}
