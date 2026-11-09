use crate::protocol::{RpcError, RpcRequest};
use crate::rpc::{PlaySource, decode_params, media_error, require_client, stream_info_json};
use crate::state::{ConnectionContext, ServerInner, lock};
use remedia_core::{CancellationToken, InputSource};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn execute(
    server: Arc<ServerInner>,
    connection: Arc<ConnectionContext>,
    request: RpcRequest,
) -> Result<Value, RpcError> {
    match request.command.as_str() {
        "media.probe.cancel" => {
            require_client(&connection)?;
            let params: ProbeCancelParams = decode_params(request.params)?;
            if let Some(token) = lock(&connection.probes).get(&params.request_id) {
                token.cancel();
            }
            Ok(json!({ "cancelled": true }))
        }
        "media.probe" => {
            require_client(&connection)?;
            let params: ProbeParams = decode_params(request.params)?;
            let timeout_ms = params.timeout_ms.unwrap_or(15_000);
            if timeout_ms == 0 || timeout_ms > u64::from(u32::MAX) {
                return Err(RpcError::new("invalid_request", "invalid probe timeoutMs"));
            }
            static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
            let permit = SLOTS.try_acquire().map_err(|_| {
                RpcError::new("resource_exhausted", "probe concurrency limit reached")
            })?;
            let cancellation = lock(&connection.probes)
                .get(&request.id)
                .cloned()
                .ok_or_else(|| RpcError::new("invalid_state", "probe was cancelled"))?;
            let _guard = ProbeCancelOnDrop(cancellation.clone());
            let source = match params.source {
                PlaySource::File { path } => {
                    let config = server.config.clone();
                    let path = tokio::task::spawn_blocking(move || config.authorize_file(&path))
                        .await
                        .map_err(|_| RpcError::new("media_failed", "file authorization failed"))?
                        .map_err(media_error)?;
                    InputSource::file(path)
                }
                PlaySource::Operation { .. } => {
                    // Operation results are single-use media grants. Probing would consume the
                    // grant, and metadata discovery must not resolve encoded audio.
                    return Err(RpcError::new(
                        "invalid_request",
                        "operation sources cannot be probed; resolve them only for playback",
                    ));
                }
                PlaySource::Url { url } => InputSource::http_url(
                    server
                        .config
                        .authorize_url(&url)
                        .await
                        .map_err(media_error)?,
                ),
            };
            let info = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                remedia_core::probe(
                    source,
                    remedia_core::ProbeOptions {
                        timeout: Duration::from_millis(timeout_ms),
                        cancellation,
                    },
                )
            })
            .await
            .map_err(|_| RpcError::new("media_failed", "probe worker failed"))?
            .map_err(|error| {
                RpcError::new(
                    match error {
                        remedia_core::DecoderError::Cancelled => "cancelled",
                        remedia_core::DecoderError::InputTimeout => "request_timeout",
                        _ => "media_failed",
                    },
                    "media probe failed",
                )
            })?;
            Ok(stream_info_json(info))
        }
        _ => Err(RpcError::new("invalid_request", "unknown probe command")),
    }
}

struct ProbeCancelOnDrop(CancellationToken);
impl Drop for ProbeCancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProbeParams {
    source: PlaySource,
    timeout_ms: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProbeCancelParams {
    request_id: String,
}
