//! Generic forwarding of allowlisted plugin-host operations.
//!
//! The server owns transport, authentication, authorization, discovery and execution.
//! Operation names, input and results are opaque JSON whose meaning belongs to the
//! application: nothing here knows about tracks, searches or metadata providers.

use std::io::Write;

use super::*;
use crate::config::valid_identifier;
use crate::plugin_host::{ExecuteOutcome, HostListing};
use crate::protocol::RpcResponse;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ListParams {
    /// Ask for a fresh manifest before answering, within the rediscovery floor.
    #[serde(default)]
    refresh: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExecuteParams {
    host_id: String,
    plugin_id: String,
    operation: String,
    input: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CancelParams {
    request_id: String,
}

pub(super) async fn execute(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    match request.command.as_str() {
        "plugin.cancel" => {
            require_client(&connection)?;

            let params: CancelParams = decode_params(request.params)?;
            // Only this connection's own pending requests are visible here, so one client can
            // never cancel another client's operation.
            let cancelled = lock(&connection.plugin_requests)
                .get(&params.request_id)
                .map(|token| token.cancel())
                .is_some();

            Ok(json!({ "cancelled": cancelled }))
        }
        "plugin.list" => {
            let params: ListParams = decode_params(request.params)?;
            let session = require_client(&connection)?;
            let cancel = request_scope(&connection, &request.id);
            let listings =
                scoped(&session, server.plugin_hosts.list(params.refresh, &cancel)).await?;

            Ok(json!({
                "hosts": listings.iter().map(HostListing::to_json).collect::<Vec<_>>(),
            }))
        }
        "plugin.execute" => {
            let params: ExecuteParams = decode_params(request.params)?;
            let session = require_client(&connection)?;

            for identifier in [&params.host_id, &params.plugin_id, &params.operation] {
                if !valid_identifier(identifier) {
                    return Err(RpcError::new(
                        "invalid_request",
                        "hostId, pluginId and operation must be valid identifiers",
                    ));
                }
            }

            let host = server.plugin_hosts.host(&params.host_id).ok_or_else(|| {
                plugin_error(PluginError::new(
                    PluginErrorKind::HostNotFound,
                    "no plugin host with that ID is configured on this node",
                ))
            })?;
            let cancel = request_scope(&connection, &request.id);
            let outcome = scoped(
                &session,
                host.execute(
                    &OperationRef {
                        plugin_id: &params.plugin_id,
                        operation: &params.operation,
                        input: &params.input,
                    },
                    &cancel,
                ),
            )
            .await?;

            forwarded_result(server.config.limits.max_message_bytes, &request.id, outcome)
        }
        _ => Err(RpcError::new("invalid_request", "unknown plugin command")),
    }
}

/// The cancellation scope registered for this request when it was admitted, which ends when
/// the connection closes, the client cancels it, or the request completes.
fn request_scope(
    connection: &ConnectionContext,
    request_id: &str,
) -> tokio_util::sync::CancellationToken {
    lock(&connection.plugin_requests)
        .get(request_id)
        .cloned()
        .unwrap_or_else(|| connection.closed.child_token())
}

/// Run a registry call, abandoning it if the owning client session closes first. Dropping
/// the future closes the in-flight HTTP request and releases the host's concurrency permit.
async fn scoped<T>(
    session: &ClientSession,
    work: impl Future<Output = Result<T, PluginError>>,
) -> Result<T, RpcError> {
    tokio::select! {
        biased;
        () = session.scope.cancelled() => Err(RpcError::new(
            "cancelled",
            "the client session closed before the operation completed",
        )),
        result = work => result.map_err(plugin_error),
    }
}

/// Build the `plugin.execute` result, refusing one which cannot be sent as a single RPC message.
fn forwarded_result(
    max_message_bytes: usize,
    request_id: &str,
    outcome: ExecuteOutcome,
) -> Result<Value, RpcError> {
    let body = match outcome {
        ExecuteOutcome::Declined => json!({ "handled": false }),
        ExecuteOutcome::Handled(result) => json!({ "handled": true, "result": result }),
    };
    let envelope = serde_json::to_vec(&RpcResponse::success(
        Some(request_id.to_owned()),
        Value::Null,
    ))
    .map_or(0, |bytes| bytes.len().saturating_sub("null".len()));

    if serialized_size_exceeds(&body, max_message_bytes.saturating_sub(envelope)) {
        return Err(plugin_error(PluginError::new(
            PluginErrorKind::ResultTooLarge,
            "plugin result exceeds the RPC message size limit",
        )));
    }

    Ok(body)
}

/// Whether the JSON encoding of `value` is longer than `limit` bytes. Stops serializing as
/// soon as the limit is crossed, so a large result costs no extra allocation.
fn serialized_size_exceeds(value: &Value, limit: usize) -> bool {
    struct Limited {
        remaining: usize,
    }

    impl Write for Limited {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if buffer.len() > self.remaining {
                return Err(std::io::Error::other("limit exceeded"));
            }

            self.remaining -= buffer.len();

            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    serde_json::to_writer(&mut Limited { remaining: limit }, value).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_check_counts_the_envelope_and_stops_at_the_limit() {
        let result = ExecuteOutcome::Handled(json!("x".repeat(100)));
        let exact = serde_json::to_vec(&RpcResponse::success(
            Some("req".into()),
            json!({ "handled": true, "result": "x".repeat(100) }),
        ))
        .unwrap()
        .len();

        assert!(
            forwarded_result(
                exact,
                "req",
                ExecuteOutcome::Handled(json!("x".repeat(100)))
            )
            .is_ok()
        );
        assert_eq!(
            forwarded_result(exact - 1, "req", result).unwrap_err().code,
            "plugin_result_too_large"
        );
    }

    #[test]
    fn unhandled_and_falsy_results_are_distinct_shapes() {
        assert_eq!(
            forwarded_result(1024, "r", ExecuteOutcome::Declined).unwrap(),
            json!({ "handled": false })
        );

        for falsy in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert_eq!(
                forwarded_result(1024, "r", ExecuteOutcome::Handled(falsy.clone())).unwrap(),
                json!({ "handled": true, "result": falsy })
            );
        }
    }
}
