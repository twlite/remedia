use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

use crate::config::{ConfigError, ServerConfig, parse_snowflake};
use crate::plugin_host::PluginHostRegistry;
use crate::protocol::{RpcResponse, parse_request};
use crate::rpc;
use crate::state::{ConnectionContext, ServerInner, new_inner};

const WS_PATH: &str = "/v1/ws";
const BOT_USER_ID_HEADER: &str = "x-remedia-bot-id";

pub struct ServerHandle {
    local_addr: SocketAddr,
    inner: Arc<ServerInner>,
    task: Mutex<Option<JoinHandle<Result<(), ServerError>>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("server configuration failed: {0}")]
    Configuration(#[from] ConfigError),
    #[error("could not initialize observability: {0}")]
    Observability(#[from] remedia_observability::ObservabilityError),
    #[error("server I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("server task failed: {0}")]
    Task(String),
}

impl ServerHandle {
    /// Bind the configured listener and start accepting authenticated WebSocket clients.
    pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
        let bind_host = config.bind_host.clone();
        let port = config.port;
        let mut observability =
            remedia_observability::Observability::new(config.observability_config()?)?;
        let shutdown = CancellationToken::new();
        let plugin_hosts =
            match PluginHostRegistry::new(&config.plugin_hosts, &observability.handle()) {
                Ok(plugin_hosts) => plugin_hosts,
                Err(_) => {
                    if let Err(error) = observability.shutdown() {
                        tracing::warn!(
                            name: "server.start.cleanup_failed",
                            target: "remedia_server::lifecycle",
                            error = %error,
                            "observability cleanup failed after plugin host initialization failed"
                        );
                    }
                    return Err(ServerError::Configuration(ConfigError::Invalid(
                        "pluginHosts HTTP client could not be initialized",
                    )));
                }
            };
        let inner = new_inner(config, plugin_hosts, observability, shutdown);
        let listener = match TcpListener::bind((bind_host.as_str(), port)).await {
            Ok(listener) => listener,
            Err(error) => {
                if let Some(mut observability) = lock(&inner.observability_owner).take()
                    && let Err(cleanup_error) = observability.shutdown()
                {
                    tracing::warn!(
                        name: "server.start.cleanup_failed",
                        target: "remedia_server::lifecycle",
                        error = %cleanup_error,
                        "observability cleanup failed after listener binding failed"
                    );
                }
                return Err(ServerError::Io(error));
            }
        };
        let local_addr = match listener.local_addr() {
            Ok(address) => address,
            Err(error) => {
                if let Some(mut observability) = lock(&inner.observability_owner).take()
                    && let Err(cleanup_error) = observability.shutdown()
                {
                    tracing::warn!(
                        name: "server.start.cleanup_failed",
                        target: "remedia_server::lifecycle",
                        error = %cleanup_error,
                        "observability cleanup failed after listener address lookup failed"
                    );
                }
                return Err(ServerError::Io(error));
            }
        };
        inner.start_plugin_hosts();
        let task_inner = Arc::clone(&inner);
        let accept = async move { accept_loop(task_inner, listener).await };
        let task = if let Some(dispatch) = inner.observability.dispatch().cloned() {
            tokio::spawn(accept.with_subscriber(dispatch))
        } else {
            tokio::spawn(accept.with_subscriber(tracing::Dispatch::none()))
        };
        Ok(Self {
            local_addr,
            inner,
            task: Mutex::new(Some(task)),
        })
    }

    /// Return the bound address. A port configured as zero reports its assigned port here.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Snapshot the same live metrics returned by the authenticated `node.stats` RPC.
    /// This is used by the local CLI discovery endpoint without exposing RPC credentials.
    pub fn statistics(&self) -> serde_json::Value {
        crate::rpc::node_stats(&self.inner)
    }

    /// Request graceful shutdown, stop all players, disconnect voice transports, and flush OTLP.
    pub async fn shutdown(&self) -> Result<(), ServerError> {
        self.inner.shutdown.cancel();
        let task = lock(&self.task).take();
        let task_result = match task {
            Some(task) => task
                .await
                .map_err(|error| ServerError::Task(error.to_string()))
                .and_then(|result| result),
            None => Ok(()),
        };

        self.inner.shutdown().await;

        let observability_result =
            if let Some(mut observability) = lock(&self.inner.observability_owner).take() {
                let flush_result = observability.force_flush();
                let shutdown_result = observability.shutdown();
                flush_result.and(shutdown_result).map_err(ServerError::from)
            } else {
                Ok(())
            };

        task_result?;
        observability_result?;
        Ok(())
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.inner.shutdown.cancel();
    }
}

async fn accept_loop(inner: Arc<ServerInner>, listener: TcpListener) -> Result<(), ServerError> {
    let mut connections = JoinSet::new();
    let mut accept_error = None;
    loop {
        tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    tracing::error!(
                        name: "server.connection_task.failed",
                        target: "remedia_server::lifecycle",
                        error = %error,
                        "WebSocket connection task failed"
                    );
                }
            },
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        accept_error = Some(error);
                        break;
                    }
                };
                let permit = match Arc::clone(&inner.connection_slots).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tokio::select! {
                            _ = inner.shutdown.cancelled() => break,
                            _ = reject_busy(stream) => {},
                        }
                        continue;
                    }
                };
                let dispatch = inner.observability.dispatch().cloned();
                let connection = handle_connection(Arc::clone(&inner), stream, permit);
                if let Some(dispatch) = dispatch {
                    connections.spawn(connection.with_subscriber(dispatch));
                } else {
                    connections.spawn(connection);
                }
            }
        }
    }

    inner.shutdown.cancel();
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            tracing::error!(
                name: "server.connection_task.failed",
                target: "remedia_server::lifecycle",
                error = %error,
                "WebSocket connection task failed during shutdown"
            );
        }
    }
    inner.shutdown().await;
    accept_error.map_or(Ok(()), |error| Err(ServerError::Io(error)))
}

async fn reject_busy(mut stream: TcpStream) {
    let response =
        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    let _ = stream.write_all(response).await;
    let _ = stream.shutdown().await;
}

// Tungstenite's handshake callback requires its concrete HTTP response as the error value.
#[allow(clippy::result_large_err)]
async fn handle_connection(
    inner: Arc<ServerInner>,
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
) {
    let authenticated = Arc::new(Mutex::new(None::<(String, u64)>));
    let handshake_auth = Arc::clone(&authenticated);
    let config = Arc::clone(&inner.config);
    let mut websocket_config = WebSocketConfig::default();
    websocket_config.max_message_size = Some(config.limits.max_message_bytes);
    websocket_config.max_frame_size = Some(config.limits.max_message_bytes);
    let request_timeout = Duration::from_millis(config.limits.request_timeout_ms);
    let websocket = tokio::select! {
        _ = inner.shutdown.cancelled() => return,
        result = tokio::time::timeout(
            request_timeout,
            accept_hdr_async_with_config(
                stream,
                move |request: &Request, response: Response| {
                    authorize_handshake(request, response, &config, &handshake_auth)
                },
                Some(websocket_config),
            ),
        ) => result,
    };
    let Ok(Ok(websocket)) = websocket else {
        return;
    };
    let Some((principal, bot_user_id)) = lock(&authenticated).take() else {
        return;
    };
    inner
        .active_connections
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let _active_connection = ActiveConnectionGuard(Arc::clone(&inner));
    let connection_id = crate::state::opaque_id(16);
    let connection = Arc::new(ConnectionContext::new(
        connection_id.clone(),
        principal,
        bot_user_id,
    ));
    tracing::info!(
        name: "server.websocket.connected",
        target: "remedia_server::connection",
        connection_id = connection_id.as_str(),
        "authenticated WebSocket connection established"
    );
    let (mut writer, mut reader) = websocket.split();
    let outbound_capacity = inner
        .config
        .limits
        .event_buffer
        .saturating_add(inner.config.limits.max_in_flight_requests)
        .saturating_add(8)
        .max(16);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(outbound_capacity);
    let connection_cancel = CancellationToken::new();
    let writer_cancel = connection_cancel.clone();
    let writer_task = tokio::spawn(async move {
        let mut interrupted = false;
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => {
                    interrupted = true;
                    break;
                }
                message = outbound_rx.recv() => match message {
                    Some(message) => {
                        let sent = tokio::select! {
                            _ = writer_cancel.cancelled() => {
                                interrupted = true;
                                None
                            }
                            result = writer.send(message) => Some(result),
                        };
                        if sent.is_none() || sent.is_some_and(|result| result.is_err()) {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        if !interrupted {
            tokio::select! {
                _ = writer_cancel.cancelled() => {},
                _ = writer.close() => {},
            }
        }
    });
    let event_task = tokio::spawn(forward_events(
        connection.active_session.subscribe(),
        outbound_tx.clone(),
        connection_cancel.clone(),
    ));
    let in_flight = Arc::new(Semaphore::new(inner.config.limits.max_in_flight_requests));
    let mut requests = JoinSet::new();
    let request_timeout = Duration::from_millis(inner.config.limits.request_timeout_ms);
    // A voice join includes WebSocket, UDP and mandatory DAVE setup. Do not apply
    // the ordinary RPC deadline to it, or the caller can leave the guild before
    // Discord has completed its handshake.
    let voice_connect_timeout = request_timeout.max(Duration::from_secs(75));

    loop {
        tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            _ = connection_cancel.cancelled() => break,
            Some(result) = requests.join_next(), if !requests.is_empty() => {
                if let Err(error) = result && !error.is_cancelled() {
                    tracing::error!(
                        name: "server.rpc.task.failed",
                        target: "remedia_server::lifecycle",
                        connection_id = connection.id.as_str(),
                        error = %error,
                        "RPC request task failed"
                    );
                }
            },
            incoming = reader.next() => {
                let Some(incoming) = incoming else { break };
                match incoming {
                    Ok(Message::Text(text)) => {
                        let payload = text.as_str().to_owned();
                        let request = match parse_request(&payload) {
                            Ok(request) => request,
                            Err(error) => {
                                let id = extract_request_id(&payload);
                                let response = RpcResponse::failure(id.clone(), error.code, error.message);
                                if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                                continue;
                            }
                        };
                        if !connection.allows_request(inner.config.limits.max_requests_per_second) {
                            let response = RpcResponse::failure(Some(request.id), "resource_exhausted", "request rate limit exceeded");
                            if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                            continue;
                        }
                        // Cancellation must remain available even when all worker slots are occupied.
                        if request.command == "media.probe.cancel" || request.command == "plugin.cancel" {
                            let id = Some(request.id.clone());
                            let response = match rpc::execute(Arc::clone(&inner), Arc::clone(&connection), request).await {
                                Ok(result) => RpcResponse::success(id, result),
                                Err(error) => RpcResponse::failure(id, error.code, error.message),
                            };
                            if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                            continue;
                        }
                        let permit = match Arc::clone(&in_flight).try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                let response = RpcResponse::failure(Some(request.id), "resource_exhausted", "too many requests are in flight");
                                if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                                continue;
                            }
                        };
                        if request.command == "media.probe" {
                            let duplicate = {
                                let mut probes = lock(&connection.probes);
                                if probes.contains_key(&request.id) { true } else {
                                    probes.insert(request.id.clone(), remedia_core::CancellationToken::new());
                                    false
                                }
                            };
                            if duplicate {
                                let response = RpcResponse::failure(Some(request.id), "invalid_request", "probe request ID is already active");
                                if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                                continue;
                            }
                        }
                        if matches!(request.command.as_str(), "plugin.list" | "plugin.execute") {
                            let duplicate = {
                                let mut pending = lock(&connection.plugin_requests);
                                if pending.contains_key(&request.id) { true } else {
                                    pending.insert(request.id.clone(), connection.closed.child_token());
                                    false
                                }
                            };
                            if duplicate {
                                let response = RpcResponse::failure(Some(request.id), "invalid_request", "plugin request ID is already active");
                                if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                                continue;
                            }
                        }
                        let inner = Arc::clone(&inner);
                        let connection = Arc::clone(&connection);
                        let outbound = outbound_tx.clone();
                        let dispatch = inner.observability.dispatch().cloned();
                        requests.spawn(async move {
                            let _permit = permit;
                            let id = Some(request.id.clone());
                            let command_name = request.command.clone();
                            let span = tracing::info_span!(
                                "server.rpc.request",
                                command = command_name.as_str(),
                                connection_id = connection.id.as_str(),
                            );
                            let execute = rpc::execute(inner.clone(), Arc::clone(&connection), request)
                                .instrument(span);
                            let execute = if let Some(dispatch) = dispatch {
                                execute.with_subscriber(dispatch)
                            } else {
                                execute.with_subscriber(tracing::Dispatch::none())
                            };
                            let deadline = if command_name == "voice.connect" {
                                voice_connect_timeout
                            } else {
                                request_timeout
                            };
                            let response = match tokio::time::timeout(deadline, execute).await {
                                Ok(Ok(result)) => RpcResponse::success(id.clone(), result),
                                Ok(Err(error)) => RpcResponse::failure(id.clone(), error.code, error.message),
                                Err(_) => RpcResponse::failure(id.clone(), "request_timeout", "request exceeded the configured timeout"),
                            };
                            if let Some(token) = lock(&connection.probes).remove(id.as_ref().expect("request id")) { token.cancel(); }
                            if let Some(token) = lock(&connection.plugin_requests).remove(id.as_ref().expect("request id")) { token.cancel(); }
                            let _ = send_response(&outbound, response).await;
                        });
                    }
                    Ok(Message::Ping(payload)) => {
                        if send_message_cancellable(
                            &outbound_tx,
                            Message::Pong(payload),
                            &connection_cancel,
                            &inner.shutdown,
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Binary(_)) => {
                        let response = RpcResponse::failure(None, "invalid_request", "only text JSON RPC messages are supported");
                        if send_response_cancellable(&outbound_tx, response, &connection_cancel, &inner.shutdown).await.is_err() { break; }
                    }
                    Ok(Message::Pong(_)) => {}
                    Err(_) => break,
                    _ => {}
                }
            }
        }
    }

    for token in lock(&connection.probes).values() {
        token.cancel();
    }
    connection_cancel.cancel();
    connection.closed.cancel();
    requests.abort_all();
    while let Some(result) = requests.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::error!(
                name: "server.rpc.task.failed",
                target: "remedia_server::lifecycle",
                connection_id = connection.id.as_str(),
                error = %error,
                "RPC request task failed during connection shutdown"
            );
        }
    }
    if let Some(session) = connection.session() {
        inner.detach_client(session, &connection.id);
    }
    let _ = outbound_tx.send(Message::Close(None)).await;
    if let Err(error) = event_task.await {
        tracing::error!(
            name: "server.event_task.failed",
            target: "remedia_server::lifecycle",
            connection_id = connection.id.as_str(),
            error = %error,
            "WebSocket event forwarding task failed"
        );
    }
    if let Err(error) = writer_task.await {
        tracing::error!(
            name: "server.writer_task.failed",
            target: "remedia_server::lifecycle",
            connection_id = connection.id.as_str(),
            error = %error,
            "WebSocket writer task failed"
        );
    }
    tracing::info!(
        name: "server.websocket.disconnected",
        target: "remedia_server::connection",
        connection_id = connection.id.as_str(),
        "WebSocket connection closed"
    );
}

struct ActiveConnectionGuard(Arc<ServerInner>);

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.0
            .active_connections
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

// The protocol API requires an HTTP Response body in the `Err` variant.
#[allow(clippy::result_large_err)]
fn authorize_handshake(
    request: &Request,
    response: Response,
    config: &ServerConfig,
    authenticated: &Arc<Mutex<Option<(String, u64)>>>,
) -> Result<Response, ErrorResponse> {
    if request.uri().path() != WS_PATH {
        return Err(http_error(
            StatusCode::NOT_FOUND,
            "unknown websocket endpoint",
        ));
    }
    let principal = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(decode_basic_credentials)
        .and_then(|(username, password)| {
            config
                .authenticate(&username, &password)
                .map(|user| user.username.clone())
        });
    let Some(principal) = principal else {
        return Err(http_error(
            StatusCode::UNAUTHORIZED,
            "authentication_failed",
        ));
    };
    let bot_user_id = request
        .headers()
        .get(BOT_USER_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_snowflake)
        .filter(|user_id| *user_id > 0);
    let Some(bot_user_id) = bot_user_id else {
        return Err(http_error(StatusCode::BAD_REQUEST, "invalid_bot_user_id"));
    };
    *lock(authenticated) = Some((principal, bot_user_id));
    Ok(response)
}

fn decode_basic_credentials(value: &str) -> Option<(String, String)> {
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username.to_owned(), password.to_owned()))
}

fn http_error(status: StatusCode, message: &str) -> ErrorResponse {
    let mut builder = tokio_tungstenite::tungstenite::http::Response::builder().status(status);
    if status == StatusCode::UNAUTHORIZED {
        builder = builder.header("WWW-Authenticate", "Basic realm=\"Remedia\"");
    }
    builder
        .body(Some(message.to_owned()))
        .expect("valid HTTP error response")
}

async fn forward_events(
    mut active: tokio::sync::watch::Receiver<Option<Arc<crate::state::ClientSession>>>,
    outbound: mpsc::Sender<Message>,
    cancellation: CancellationToken,
) {
    loop {
        let Some(session) = active.borrow().clone() else {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                changed = active.changed() => if changed.is_err() { return; },
            }
            continue;
        };
        let mut events = session.subscribe();
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                changed = active.changed() => {
                    if changed.is_err() { return; }
                    break;
                }
                result = events.recv() => {
                    let event = match result {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                            let sequence = session.event_sequence.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
                            crate::protocol::event(
                                "client.events_lost",
                                &session.id,
                                sequence,
                                serde_json::json!({"dropped": dropped}),
                            )
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    let text = event.to_string();
                    if outbound.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

async fn send_response(outbound: &mpsc::Sender<Message>, response: RpcResponse) -> Result<(), ()> {
    let text = serde_json::to_string(&response).map_err(|_| ())?;
    outbound
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn send_response_cancellable(
    outbound: &mpsc::Sender<Message>,
    response: RpcResponse,
    cancellation: &CancellationToken,
    shutdown: &CancellationToken,
) -> Result<(), ()> {
    let text = serde_json::to_string(&response).map_err(|_| ())?;
    send_message_cancellable(outbound, Message::Text(text.into()), cancellation, shutdown).await
}

async fn send_message_cancellable(
    outbound: &mpsc::Sender<Message>,
    message: Message,
    cancellation: &CancellationToken,
    shutdown: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(()),
        _ = shutdown.cancelled() => Err(()),
        result = outbound.send(message) => result.map_err(|_| ()),
    }
}

fn extract_request_id(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .get("id")?
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .map(ToOwned::to_owned)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::decode_basic_credentials;

    #[test]
    fn basic_credentials_split_on_first_colon_and_reject_invalid_encoding() {
        assert_eq!(
            decode_basic_credentials("Basic dXNlcjpwYXNzOndvcmQ="),
            Some(("user".into(), "pass:word".into()))
        );
        assert_eq!(decode_basic_credentials("Bearer token"), None);
        assert_eq!(decode_basic_credentials("Basic !"), None);
    }
}
