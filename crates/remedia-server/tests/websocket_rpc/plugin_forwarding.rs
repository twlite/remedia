//! Generic plugin-operation forwarding through the authenticated WebSocket RPC.
//!
//! The plugin host is a small local HTTP fixture speaking the Remedia v1 contract. Its
//! behavior is selected by the `mode` field of each operation's input, so one allowlisted
//! operation can exercise every outcome without any provider-specific logic.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::*;

const TOKEN: &str = "mock-host-token-0123456789abcdef";

#[derive(Default)]
struct MockState {
    /// `(pluginId, operation)` pairs the manifest advertises.
    advertised: Mutex<Vec<(String, String)>>,
    /// Answer every connection by dropping it, as a stopped host would.
    down: AtomicBool,
    manifest_calls: AtomicUsize,
    execute_calls: AtomicUsize,
    media_requests: AtomicUsize,
    /// Execute requests currently being handled.
    in_flight: AtomicUsize,
    /// Execute requests whose connection the server closed before a response was sent.
    closed_early: AtomicUsize,
    inputs: Mutex<Vec<Value>>,
    authorization: Mutex<Vec<String>>,
    operation_ids: Mutex<Vec<String>>,
    counters: Mutex<HashMap<String, usize>>,
}

struct MockHost {
    address: SocketAddr,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockHost {
    async fn start(advertised: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(MockState::default());

        *state.advertised.lock().unwrap() = advertised
            .iter()
            .map(|(plugin, operation)| ((*plugin).to_owned(), (*operation).to_owned()))
            .collect();

        let accept_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&accept_state)));
            }
        });

        Self {
            address,
            state,
            task,
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    fn set_advertised(&self, advertised: &[(&str, &str)]) {
        *self.state.advertised.lock().unwrap() = advertised
            .iter()
            .map(|(plugin, operation)| ((*plugin).to_owned(), (*operation).to_owned()))
            .collect();
    }

    fn count(&self, counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    async fn wait_for(&self, counter: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while counter.load(Ordering::SeqCst) != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the mock host did not reach the expected state");
    }
}

impl Drop for MockHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(stream: TcpStream, state: Arc<MockState>) {
    if state.down.load(Ordering::SeqCst) {
        return;
    }

    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    loop {
        let mut line = String::new();

        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }

        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut length = 0_usize;
        let mut authorization = String::new();
        let mut operation_id = String::new();

        loop {
            let mut header = String::new();

            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                return;
            }

            let header = header.trim_end();

            if header.is_empty() {
                break;
            }

            if let Some((name, value)) = header.split_once(':') {
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap_or(0),
                    "authorization" => authorization = value.trim().to_owned(),
                    "x-remedia-operation-id" => operation_id = value.trim().to_owned(),
                    _ => {}
                }
            }
        }

        let mut body = vec![0_u8; length];

        if reader.read_exact(&mut body).await.is_err() {
            return;
        }

        if state.down.load(Ordering::SeqCst) {
            return;
        }

        let response = if authorization != format!("Bearer {TOKEN}") {
            http(401, json!({"error": {"code": "unauthorized"}}).to_string())
        } else if method == "GET" && path == "/.well-known/remedia" {
            state.manifest_calls.fetch_add(1, Ordering::SeqCst);
            http(200, manifest(&state))
        } else if method == "POST" && path == "/remedia/v1/execute" {
            state.authorization.lock().unwrap().push(authorization);
            state.operation_ids.lock().unwrap().push(operation_id);

            match execute(&state, &body, &mut reader).await {
                Some(response) => response,
                None => return,
            }
        } else {
            state.media_requests.fetch_add(1, Ordering::SeqCst);
            http(404, String::new())
        };

        if write.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn http(status: u16, body: String) -> String {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn manifest(state: &MockState) -> String {
    let mut plugins: HashMap<String, Vec<String>> = HashMap::new();

    for (plugin, operation) in state.advertised.lock().unwrap().iter() {
        plugins
            .entry(plugin.clone())
            .or_default()
            .push(operation.clone());
    }

    json!({
        "name": "Mock host",
        "protocolVersion": 1,
        "endpoints": {"execute": "/remedia/v1/execute", "media": "/remedia/v1/media/{token}"},
        "plugins": plugins.into_iter().map(|(id, operations)| json!({"id": id, "operations": operations})).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Returns `None` when the server closed the connection before the host answered.
async fn execute(
    state: &MockState,
    body: &[u8],
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<String> {
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let input = request["input"].clone();

    state.execute_calls.fetch_add(1, Ordering::SeqCst);
    state.inputs.lock().unwrap().push(input.clone());
    state.in_flight.fetch_add(1, Ordering::SeqCst);

    let outcome = respond(state, &input, reader).await;

    state.in_flight.fetch_sub(1, Ordering::SeqCst);

    if outcome.is_none() {
        state.closed_early.fetch_add(1, Ordering::SeqCst);
    }

    outcome
}

async fn respond(
    state: &MockState,
    input: &Value,
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<String> {
    let handled = |result: Value| http(200, json!({"handled": true, "result": result}).to_string());

    Some(match input["mode"].as_str().unwrap_or("echo") {
        "echo" => handled(input.clone()),
        "value" => handled(input["value"].clone()),
        "unhandled" => http(200, json!({"handled": false}).to_string()),
        "status" => http(
            input["status"].as_u64().unwrap_or(500) as u16,
            json!({"error": {"code": "mock"}}).to_string(),
        ),
        "malformed" => http(200, "this is not json".to_owned()),
        "no-result" => http(200, json!({"handled": true}).to_string()),
        "big" => handled(json!(
            "x".repeat(input["bytes"].as_u64().unwrap_or(0) as usize)
        )),
        "descriptor" => handled(json!({"url": "http://127.0.0.1:9/never-fetched"})),
        // A media descriptor for `input.url`. With `hangAfter`, calls after that many never
        // answer, as a slow provider would while a seek is waiting on it.
        "media-url" => {
            let key = input["key"].as_str().unwrap_or("media").to_owned();
            let seen = {
                let mut counters = state.counters.lock().unwrap();
                let seen = counters.entry(key).or_insert(0);

                *seen += 1;
                *seen
            };

            if input["hangAfter"]
                .as_u64()
                .is_some_and(|after| seen as u64 > after)
            {
                let mut scratch = [0_u8; 64];
                let _ = reader.read(&mut scratch).await;

                return None;
            }

            handled(json!({"url": input["url"]}))
        }
        "slow" => {
            tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap_or(100))).await;
            handled(input.clone())
        }
        "fail-first" => {
            let key = input["key"].as_str().unwrap_or("default").to_owned();
            let seen = {
                let mut counters = state.counters.lock().unwrap();
                let seen = counters.entry(key).or_insert(0);

                *seen += 1;
                *seen
            };

            if seen <= input["times"].as_u64().unwrap_or(1) as usize {
                http(503, json!({"error": {"code": "busy"}}).to_string())
            } else {
                handled(input.clone())
            }
        }
        "hang" => {
            // The only way out is the peer closing the connection, which is how a cancelled
            // or timed-out request is observed from the host's side.
            let mut scratch = [0_u8; 64];

            match reader.read(&mut scratch).await {
                Ok(0) | Err(_) => return None,
                Ok(_) => return None,
            }
        }
        _ => http(400, json!({"error": {"code": "bad_mode"}}).to_string()),
    })
}

fn host_config(id: &str, host: &MockHost, operations: Value, limits: Value) -> Value {
    json!({
        "id": id,
        "url": host.origin(),
        "auth": {"type": "bearer", "token": TOKEN},
        "allowHttp": true,

        "operations": operations,
        "limits": limits,
    })
}

fn ops(entries: &[(&str, &str, bool)]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|(plugin, operation, replay_safe)| {
                json!({"pluginId": plugin, "operation": operation, "replaySafe": replay_safe})
            })
            .collect(),
    )
}

async fn send(socket: &mut TestSocket, id: &str, command: &str, params: Value) {
    socket
        .send(Message::Text(
            json!({"version": 1, "id": id, "command": command, "params": params})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}

/// Read until every listed response has arrived, in whatever order the server sent them.
async fn receive_many(socket: &mut TestSocket, ids: &[&str]) -> HashMap<String, Value> {
    let mut found = HashMap::new();

    tokio::time::timeout(Duration::from_secs(10), async {
        while found.len() < ids.len() {
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();

            if let Some(id) = value.get("id").and_then(Value::as_str)
                && ids.contains(&id)
            {
                found.insert(id.to_owned(), value);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for responses {ids:?}"));

    found
}

/// Wait until the host reports no pending operations, as permits are released asynchronously.
async fn wait_for_idle(socket: &mut TestSocket, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stats = plugin_stats(socket, id).await;

            if stats["resources"]["pending"] == 0 {
                return stats;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("pending plugin operations were not released")
}

fn execute_params(host_id: &str, operation: &str, input: Value) -> Value {
    json!({"hostId": host_id, "pluginId": "example.metadata", "operation": operation, "input": input})
}

async fn session(server: &RunningServer, user: (&str, &str, &str)) -> TestSocket {
    let mut socket = server.connect_as(user.0, user.1, user.2).await.unwrap();

    create_client(&mut socket).await;

    socket
}

async fn plugin_stats(socket: &mut TestSocket, id: &str) -> Value {
    let stats = rpc(socket, "stats", "node.stats", json!({})).await;

    stats["result"]["pluginHosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|host| host["id"] == id)
        .cloned()
        .unwrap()
}

const BOT_A: (&str, &str, &str) = ("bot-a", "password-a", BOT_A_ID);
const BOT_B: (&str, &str, &str) = ("bot-b", "password-b", BOT_B_ID);

async fn single_host_server(
    host: &MockHost,
    operations: Value,
    host_limits: Value,
    server_limits: Value,
) -> RunningServer {
    RunningServer::start_with_plugin_hosts(
        json!([host_config("meta-host", host, operations, host_limits)]),
        server_limits,
    )
    .await
}

#[tokio::test]
async fn listing_exposes_only_allowed_and_advertised_operations_in_stable_order() {
    let zeta = MockHost::start(&[
        ("example.metadata", "lyrics.search"),
        ("example.metadata", "catalog.search"),
        ("example.metadata", "not.allowed"),
    ])
    .await;
    let alpha = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let down = MockHost::start(&[]).await;

    down.state.down.store(true, Ordering::SeqCst);

    let server = RunningServer::start_with_plugin_hosts(
        json!([
            host_config(
                "zeta",
                &zeta,
                ops(&[
                    ("example.metadata", "lyrics.search", true),
                    ("example.metadata", "catalog.search", true),
                    ("example.metadata", "allowed.but.missing", true),
                ]),
                json!({})
            ),
            host_config(
                "alpha",
                &alpha,
                ops(&[("example.metadata", "catalog.search", false)]),
                json!({})
            ),
            host_config(
                "down",
                &down,
                ops(&[("example.metadata", "catalog.search", true)]),
                json!({"requestTimeoutMs": 500})
            ),
        ]),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let listing = rpc(&mut socket, "list", "plugin.list", json!({})).await;

    assert_eq!(listing["ok"], true, "{listing}");
    let hosts = listing["result"]["hosts"].as_array().unwrap();

    assert_eq!(
        hosts
            .iter()
            .map(|host| host["hostId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha", "down", "zeta"]
    );
    assert_eq!(
        hosts[0],
        json!({"hostId": "alpha", "status": "healthy", "operations": [
            {"pluginId": "example.metadata", "operation": "catalog.search"}
        ]})
    );
    assert_eq!(
        hosts[2],
        json!({"hostId": "zeta", "status": "healthy", "operations": [
            {"pluginId": "example.metadata", "operation": "catalog.search"},
            {"pluginId": "example.metadata", "operation": "lyrics.search"}
        ]})
    );
    // An unreachable host is reported honestly and advertises nothing.
    assert_ne!(hosts[1]["status"], "healthy");
    assert_eq!(hosts[1]["operations"], json!([]));

    // Neither deployment details nor credentials may be visible to a client.
    let serialized = listing.to_string();

    for secret in [TOKEN, "127.0.0.1", &alpha.address.port().to_string()] {
        assert!(!serialized.contains(secret), "leaked {secret}");
    }

    let info = rpc(&mut socket, "info", "node.info", json!({})).await;

    assert_eq!(
        info["result"]["capabilities"]["pluginOperations"],
        json!({"available": true, "hosts": ["zeta", "alpha", "down"], "forwarding": {"available": true}})
    );

    server.shutdown().await;
}

#[tokio::test]
async fn plugin_commands_require_a_client_session() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({}),
        json!({}),
    )
    .await;
    let mut socket = server.connect_as(BOT_A.0, BOT_A.1, BOT_A.2).await.unwrap();

    for (command, params) in [
        ("plugin.list", json!({})),
        (
            "plugin.execute",
            execute_params("meta-host", "catalog.search", json!({})),
        ),
        ("plugin.cancel", json!({"requestId": "x"})),
    ] {
        let response = rpc(&mut socket, command, command, params).await;

        assert_eq!(response["error"]["code"], "unknown_session", "{response}");
    }

    assert_eq!(host.count(&host.state.execute_calls), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn execute_round_trips_json_and_keeps_handled_and_unhandled_distinct() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let input =
        json!({"query": "some query", "nested": [1, null, {"deep": true}], "unicode": "é✓"});
    let echoed = rpc(
        &mut socket,
        "echo",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", input.clone()),
    )
    .await;

    assert_eq!(
        echoed["result"],
        json!({"handled": true, "result": input}),
        "{echoed}"
    );
    assert_eq!(host.state.inputs.lock().unwrap()[0], input);

    for (index, value) in [
        json!(null),
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
        json!({"items": []}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("value-{index}");
        let response = rpc(
            &mut socket,
            &id,
            "plugin.execute",
            execute_params(
                "meta-host",
                "catalog.search",
                json!({"mode": "value", "value": value}),
            ),
        )
        .await;

        assert_eq!(
            response["result"],
            json!({"handled": true, "result": value}),
            "{response}"
        );
    }

    let unhandled = rpc(
        &mut socket,
        "unhandled",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "unhandled"})),
    )
    .await;

    assert_eq!(unhandled["ok"], true, "{unhandled}");
    assert_eq!(unhandled["result"], json!({"handled": false}));

    // A host which sends a media descriptor as a generic result gets it back untouched.
    let descriptor = rpc(
        &mut socket,
        "descriptor",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "descriptor"})),
    )
    .await;

    assert_eq!(
        descriptor["result"]["result"]["url"],
        "http://127.0.0.1:9/never-fetched"
    );
    assert_eq!(host.count(&host.state.media_requests), 0);

    let authorizations = host.state.authorization.lock().unwrap().clone();

    assert!(
        authorizations
            .iter()
            .all(|value| value == &format!("Bearer {TOKEN}"))
    );
    assert!(
        host.state
            .operation_ids
            .lock()
            .unwrap()
            .iter()
            .all(|id| id.len() == 32)
    );

    // Generic operations are counted separately and never touch media resources.
    let stats = plugin_stats(&mut socket, "meta-host").await;

    assert_eq!(stats["totals"]["genericOperations"], 10);
    assert_eq!(stats["totals"]["genericUnhandled"], 1);
    assert_eq!(stats["totals"]["genericFailures"], 0);
    assert_eq!(stats["totals"]["operations"], 0);
    assert_eq!(stats["totals"]["grantsOpened"], 0);
    assert_eq!(stats["totals"]["mediaBytes"], 0);
    assert_eq!(stats["resources"]["activeStreams"], 0);
    assert_eq!(stats["resources"]["pending"], 0);
    assert_eq!(stats["status"]["state"], "healthy");

    server.shutdown().await;
}

#[tokio::test]
async fn invalid_unknown_and_disallowed_requests_never_reach_the_host() {
    let host = MockHost::start(&[
        ("example.metadata", "catalog.search"),
        ("example.metadata", "unlisted"),
    ])
    .await;
    // `allowed.but.missing` is configured yet not advertised.
    let server = single_host_server(
        &host,
        ops(&[
            ("example.metadata", "catalog.search", true),
            ("example.metadata", "allowed.but.missing", true),
        ]),
        json!({"maxRequestBytes": 256}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let cases = [
        (
            "unknown-host",
            execute_params("other", "catalog.search", json!({})),
            "plugin_host_not_found",
        ),
        (
            "disallowed",
            execute_params("meta-host", "unlisted", json!({})),
            "plugin_operation_not_allowed",
        ),
        (
            "unadvertised",
            execute_params("meta-host", "allowed.but.missing", json!({})),
            "plugin_operation_unavailable",
        ),
        (
            "bad-identifier",
            execute_params("meta-host", "../escape", json!({})),
            "invalid_request",
        ),
        (
            "bad-host-identifier",
            execute_params("meta host", "catalog.search", json!({})),
            "invalid_request",
        ),
        (
            "missing-input",
            json!({"hostId": "meta-host", "pluginId": "example.metadata", "operation": "catalog.search"}),
            "invalid_request",
        ),
        (
            "unknown-field",
            json!({"hostId": "meta-host", "pluginId": "example.metadata", "operation": "catalog.search", "input": {}, "extra": 1}),
            "invalid_request",
        ),
        (
            "oversized-input",
            execute_params(
                "meta-host",
                "catalog.search",
                json!({"padding": "x".repeat(1_000)}),
            ),
            "invalid_request",
        ),
    ];

    for (id, params, code) in cases {
        let response = rpc(&mut socket, id, "plugin.execute", params).await;

        assert_eq!(response["ok"], false, "{id}: {response}");
        assert_eq!(response["error"]["code"], code, "{id}: {response}");
    }

    assert_eq!(host.count(&host.state.execute_calls), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn host_failures_map_to_typed_errors_without_misreporting_health() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({"maxResponseBytes": 2048, "requestTimeoutMs": 400, "maxAttempts": 3}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;

    for (id, input, code, calls) in [
        (
            "unauthorized",
            json!({"mode": "status", "status": 401}),
            "plugin_host_unauthorized",
            1,
        ),
        (
            "rejected",
            json!({"mode": "status", "status": 422}),
            "invalid_request",
            1,
        ),
        (
            "malformed",
            json!({"mode": "malformed"}),
            "plugin_operation_failed",
            1,
        ),
        (
            "no-result",
            json!({"mode": "no-result"}),
            "plugin_operation_failed",
            1,
        ),
        (
            "oversized-output",
            json!({"mode": "big", "bytes": 10_000}),
            "plugin_operation_failed",
            1,
        ),
        ("timeout", json!({"mode": "hang"}), "request_timeout", 3),
    ] {
        let before = host.count(&host.state.execute_calls);
        let response = rpc(
            &mut socket,
            id,
            "plugin.execute",
            execute_params("meta-host", "catalog.search", input),
        )
        .await;

        assert_eq!(response["error"]["code"], code, "{id}: {response}");
        // Authentication, validation and protocol failures are never replayed; a timeout of a
        // replay-safe operation is retried up to maxAttempts.
        assert_eq!(
            host.count(&host.state.execute_calls) - before,
            calls,
            "{id}"
        );
        assert!(
            !response
                .to_string()
                .contains(&host.address.port().to_string()),
            "{response}"
        );
    }

    let stats = plugin_stats(&mut socket, "meta-host").await;

    assert_eq!(stats["totals"]["unauthorized"], 1);
    assert_eq!(stats["totals"]["timeouts"], 3);
    assert_eq!(stats["totals"]["genericOperations"], 6);
    assert_eq!(stats["totals"]["genericFailures"], 4);
    assert_eq!(stats["totals"]["operationFailures"], 0);
    wait_for_idle(&mut socket, "meta-host").await;

    // A host which stopped answering is a transport failure, not an application error.
    host.state.down.store(true, Ordering::SeqCst);

    let down = rpc(
        &mut socket,
        "down",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(down["error"]["code"], "plugin_host_unavailable", "{down}");
    server.shutdown().await;
}

#[tokio::test]
async fn results_which_do_not_fit_one_rpc_message_are_rejected_without_closing_the_socket() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({"maxResponseBytes": 1_048_576}),
        json!({"maxMessageBytes": 4_096}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let large = rpc(
        &mut socket,
        "large",
        "plugin.execute",
        execute_params(
            "meta-host",
            "catalog.search",
            json!({"mode": "big", "bytes": 20_000}),
        ),
    )
    .await;

    assert_eq!(large["error"]["code"], "plugin_result_too_large", "{large}");

    let small = rpc(
        &mut socket,
        "small",
        "plugin.execute",
        execute_params(
            "meta-host",
            "catalog.search",
            json!({"mode": "big", "bytes": 1_000}),
        ),
    )
    .await;

    assert_eq!(small["ok"], true, "{small}");
    server.shutdown().await;
}

#[tokio::test]
async fn only_replay_safe_operations_are_retried() {
    let host =
        MockHost::start(&[("example.metadata", "safe"), ("example.metadata", "unsafe")]).await;
    let server = single_host_server(
        &host,
        ops(&[
            ("example.metadata", "safe", true),
            ("example.metadata", "unsafe", false),
        ]),
        json!({"maxAttempts": 2}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let safe = rpc(
        &mut socket,
        "safe",
        "plugin.execute",
        execute_params(
            "meta-host",
            "safe",
            json!({"mode": "fail-first", "key": "safe", "times": 1}),
        ),
    )
    .await;

    assert_eq!(safe["result"]["handled"], true, "{safe}");
    assert_eq!(host.count(&host.state.execute_calls), 2);

    let unsafe_operation = rpc(
        &mut socket,
        "unsafe",
        "plugin.execute",
        execute_params(
            "meta-host",
            "unsafe",
            json!({"mode": "fail-first", "key": "unsafe", "times": 1}),
        ),
    )
    .await;

    assert_eq!(
        unsafe_operation["error"]["code"], "plugin_host_unavailable",
        "{unsafe_operation}"
    );
    assert_eq!(host.count(&host.state.execute_calls), 3);

    let stats = plugin_stats(&mut socket, "meta-host").await;

    assert_eq!(stats["totals"]["retries"], 1);
    server.shutdown().await;
}

#[tokio::test]
async fn cancelling_aborts_the_http_request_and_releases_the_concurrency_permit() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({"maxPendingOperations": 1}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;

    send(
        &mut socket,
        "pending",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "hang"})),
    )
    .await;
    host.wait_for(&host.state.in_flight, 1).await;

    // The host's only permit is held, so a second operation is refused rather than queued.
    let refused = rpc(
        &mut socket,
        "refused",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(refused["error"]["code"], "resource_exhausted", "{refused}");

    send(
        &mut socket,
        "cancel",
        "plugin.cancel",
        json!({"requestId": "pending"}),
    )
    .await;

    let responses = receive_many(&mut socket, &["cancel", "pending"]).await;

    assert_eq!(
        responses["cancel"]["result"],
        json!({"cancelled": true}),
        "{responses:?}"
    );
    assert_eq!(
        responses["pending"]["error"]["code"], "cancelled",
        "{responses:?}"
    );
    host.wait_for(&host.state.closed_early, 1).await;

    let stats = wait_for_idle(&mut socket, "meta-host").await;

    assert_eq!(stats["totals"]["cancelled"], 1);

    let after = rpc(
        &mut socket,
        "after",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(after["ok"], true, "{after}");

    // Cancelling something which already finished, or never existed, is a harmless no-op.
    let stale = rpc(
        &mut socket,
        "stale",
        "plugin.cancel",
        json!({"requestId": "pending"}),
    )
    .await;

    assert_eq!(stale["result"], json!({"cancelled": false}));
    server.shutdown().await;
}

#[tokio::test]
async fn cancellation_is_scoped_to_the_requesting_connection() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({}),
        json!({}),
    )
    .await;
    let mut owner = session(&server, BOT_A).await;
    let mut same_principal = session(&server, BOT_A).await;
    let mut other_principal = session(&server, BOT_B).await;

    send(
        &mut owner,
        "shared-id",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "hang"})),
    )
    .await;
    host.wait_for(&host.state.in_flight, 1).await;

    for socket in [&mut same_principal, &mut other_principal] {
        let attempt = rpc(
            socket,
            "intruder",
            "plugin.cancel",
            json!({"requestId": "shared-id"}),
        )
        .await;

        assert_eq!(attempt["result"], json!({"cancelled": false}), "{attempt}");
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(host.count(&host.state.in_flight), 1);
    assert_eq!(host.count(&host.state.closed_early), 0);

    // Closing the owner's socket is what finally ends the operation, and nothing leaks.
    owner.close(None).await.unwrap();
    host.wait_for(&host.state.closed_early, 1).await;

    wait_for_idle(&mut other_principal, "meta-host").await;
    server.shutdown().await;
}

#[tokio::test]
async fn closing_the_client_session_cancels_its_pending_operations() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;

    send(
        &mut socket,
        "pending",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "hang"})),
    )
    .await;
    host.wait_for(&host.state.in_flight, 1).await;

    send(&mut socket, "close", "client.close", json!({})).await;

    let responses = receive_many(&mut socket, &["close", "pending"]).await;

    assert_eq!(responses["close"]["ok"], true, "{responses:?}");
    assert_eq!(
        responses["pending"]["error"]["code"], "cancelled",
        "{responses:?}"
    );
    host.wait_for(&host.state.closed_early, 1).await;
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_pending_operations() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;

    send(
        &mut socket,
        "pending",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "hang"})),
    )
    .await;
    host.wait_for(&host.state.in_flight, 1).await;
    tokio::time::timeout(Duration::from_secs(10), server.shutdown())
        .await
        .expect("shutdown must not wait for a hung plugin operation");
    host.wait_for(&host.state.closed_early, 1).await;
}

#[tokio::test]
async fn the_server_deadline_ends_a_stuck_operation_and_frees_its_resources() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", false)]),
        json!({"requestTimeoutMs": 60_000, "maxPendingOperations": 1}),
        json!({"requestTimeoutMs": 500}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let timed_out = rpc(
        &mut socket,
        "stuck",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({"mode": "hang"})),
    )
    .await;

    assert_eq!(timed_out["error"]["code"], "request_timeout", "{timed_out}");
    host.wait_for(&host.state.closed_early, 1).await;

    wait_for_idle(&mut socket, "meta-host").await;
    server.shutdown().await;
}

#[tokio::test]
async fn concurrent_operations_are_independent() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({"maxPendingOperations": 8}),
        json!({"maxInFlightRequests": 8}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;

    for index in 0..6 {
        send(
            &mut socket,
            &format!("request-{index}"),
            "plugin.execute",
            execute_params(
                "meta-host",
                "catalog.search",
                json!({"mode": "slow", "ms": 150, "index": index}),
            ),
        )
        .await;
    }

    // Concurrent operations can complete out of order. Collect all replies
    // without discarding another request's response while waiting for one ID.
    let request_ids = (0..6)
        .map(|index| format!("request-{index}"))
        .collect::<Vec<_>>();
    let expected_ids = request_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let responses = receive_many(&mut socket, &expected_ids).await;

    for index in 0..6 {
        let response = &responses[&format!("request-{index}")];

        assert_eq!(response["ok"], true, "{response}");
        // Every response carries its own request's input, never another's.
        assert_eq!(response["result"]["result"]["index"], index);
    }

    let stats = plugin_stats(&mut socket, "meta-host").await;

    assert_eq!(stats["totals"]["genericOperations"], 6);
    assert_eq!(stats["resources"]["pending"], 0);
    server.shutdown().await;
}

fn listed_operations(listing: &Value) -> Vec<String> {
    listing["result"]["hosts"][0]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|operation| operation["operation"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn repeated_listings_reuse_the_cached_manifest_and_refresh_is_rate_limited() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[("example.metadata", "catalog.search", true)]),
        json!({}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let first = rpc(&mut socket, "first", "plugin.list", json!({})).await;

    assert_eq!(listed_operations(&first), ["catalog.search"]);

    let fetched = host.count(&host.state.manifest_calls);

    for index in 0..5 {
        rpc(
            &mut socket,
            &format!("again-{index}"),
            "plugin.list",
            json!({}),
        )
        .await;
        rpc(
            &mut socket,
            &format!("refresh-{index}"),
            "plugin.list",
            json!({"refresh": true}),
        )
        .await;
        rpc(
            &mut socket,
            &format!("execute-{index}"),
            "plugin.execute",
            execute_params("meta-host", "catalog.search", json!({})),
        )
        .await;
    }

    // Neither listings, forced refreshes inside the rediscovery floor, nor executions refetch.
    assert_eq!(host.count(&host.state.manifest_calls), fetched);
    server.shutdown().await;
}

#[tokio::test]
async fn a_restarted_host_with_different_operations_is_rediscovered() {
    let host = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let server = single_host_server(
        &host,
        ops(&[
            ("example.metadata", "catalog.search", true),
            ("example.metadata", "lyrics.search", true),
        ]),
        json!({"discoveryTtlMs": 1_000, "requestTimeoutMs": 500}),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let first = rpc(&mut socket, "first", "plugin.list", json!({})).await;

    assert_eq!(listed_operations(&first), ["catalog.search"]);

    // The host stops: it must not keep advertising operations from a stale manifest.
    host.state.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1_300)).await;

    let down = rpc(&mut socket, "down", "plugin.list", json!({})).await;

    assert!(listed_operations(&down).is_empty(), "{down}");
    assert_ne!(down["result"]["hosts"][0]["status"], "healthy");

    let unavailable = rpc(
        &mut socket,
        "execute-down",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(unavailable["ok"], false, "{unavailable}");

    // It returns advertising a different set; the next freshness window picks that up.
    host.set_advertised(&[("example.metadata", "lyrics.search")]);
    host.state.down.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1_300)).await;

    let back = rpc(&mut socket, "back", "plugin.list", json!({})).await;

    assert_eq!(listed_operations(&back), ["lyrics.search"], "{back}");

    let executable = rpc(
        &mut socket,
        "execute-new",
        "plugin.execute",
        execute_params("meta-host", "lyrics.search", json!({})),
    )
    .await;

    assert_eq!(executable["ok"], true, "{executable}");

    let removed = rpc(
        &mut socket,
        "execute-removed",
        "plugin.execute",
        execute_params("meta-host", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(
        removed["error"]["code"], "plugin_operation_unavailable",
        "{removed}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn disabled_hosts_are_never_contacted_and_never_replaced_by_another_host() {
    let disabled = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let enabled = MockHost::start(&[("example.metadata", "catalog.search")]).await;
    let allowed = ops(&[("example.metadata", "catalog.search", true)]);
    let mut off = host_config("off-host", &disabled, allowed.clone(), json!({}));

    off["enabled"] = json!(false);

    let server = RunningServer::start_with_plugin_hosts(
        json!([off, host_config("on-host", &enabled, allowed, json!({}))]),
        json!({}),
    )
    .await;
    let mut socket = session(&server, BOT_A).await;
    let info = rpc(&mut socket, "info", "node.info", json!({})).await;

    // A disabled host is neither advertised nor discoverable...
    assert_eq!(
        info["result"]["capabilities"]["pluginOperations"]["hosts"],
        json!(["on-host"])
    );

    let listing = rpc(&mut socket, "list", "plugin.list", json!({})).await;

    assert_eq!(listing["result"]["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(listing["result"]["hosts"][0]["hostId"], "on-host");

    // ...and requests naming it fail clearly instead of falling back to the other host.
    let execute = rpc(
        &mut socket,
        "execute",
        "plugin.execute",
        execute_params("off-host", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(
        execute["error"]["code"], "plugin_host_disabled",
        "{execute}"
    );

    let guild = "200000000000000001";

    rpc(
        &mut socket,
        "guild",
        "guild.create",
        json!({"guildId": guild}),
    )
    .await;

    let play = rpc(
        &mut socket,
        "play",
        "player.play",
        json!({"guildId": guild, "source": {
            "type": "operation", "hostId": "off-host",
            "pluginId": "example.metadata", "operation": "catalog.search", "input": {}
        }}),
    )
    .await;

    assert_eq!(play["error"]["code"], "plugin_host_disabled", "{play}");

    let unknown = rpc(
        &mut socket,
        "unknown",
        "plugin.execute",
        execute_params("never-configured", "catalog.search", json!({})),
    )
    .await;

    assert_eq!(unknown["error"]["code"], "plugin_host_not_found");
    assert_eq!(disabled.count(&disabled.state.manifest_calls), 0);
    assert_eq!(disabled.count(&disabled.state.execute_calls), 0);
    assert_eq!(enabled.count(&enabled.state.execute_calls), 0);

    let stats = rpc(&mut socket, "stats", "node.stats", json!({})).await;

    assert_eq!(stats["result"]["pluginHosts"].as_array().unwrap().len(), 1);
    server.shutdown().await;
}

mod reopening {
    //! A guild that opts in to fallback seeking reopens a plugin-hosted track by running its
    //! operation again. That second run is an ordinary operation: same host, same credentials,
    //! same input, the host's limits, and cancellable.

    use super::super::seeking::{Origin, silent_wave};
    use super::*;
    use std::time::Instant;

    const GUILD: &str = "400000000000000001";

    struct MediaOrigin {
        url: String,
        requests: Arc<AtomicUsize>,
        interrupted: Arc<AtomicUsize>,
        shutdown: tokio_util::sync::CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl MediaOrigin {
        async fn start(body: Vec<u8>) -> Self {
            let declared_length = body.len();
            Self::start_with_declared_length(body, declared_length).await
        }

        async fn start_with_declared_length(body: Vec<u8>, declared_length: usize) -> Self {
            Self::start_with_delay(body, declared_length, Duration::ZERO).await
        }

        async fn start_delayed(body: Vec<u8>, initial_delay: Duration) -> Self {
            let declared_length = body.len();
            Self::start_with_delay(body, declared_length, initial_delay).await
        }

        async fn start_with_delay(
            body: Vec<u8>,
            declared_length: usize,
            initial_delay: Duration,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/media.wav", listener.local_addr().unwrap());
            let body = Arc::new(body);
            let requests = Arc::new(AtomicUsize::new(0));
            let interrupted = Arc::new(AtomicUsize::new(0));
            let shutdown = tokio_util::sync::CancellationToken::new();
            let accept_shutdown = shutdown.clone();
            let accept_body = Arc::clone(&body);
            let accept_declared_length = declared_length;
            let accept_requests = Arc::clone(&requests);
            let accept_interrupted = Arc::clone(&interrupted);
            let task = tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        () = accept_shutdown.cancelled() => break,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((stream, _)) = accepted else {
                        break;
                    };

                    tokio::spawn(serve_media(
                        stream,
                        Arc::clone(&accept_body),
                        accept_declared_length,
                        initial_delay,
                        Arc::clone(&accept_requests),
                        Arc::clone(&accept_interrupted),
                        accept_shutdown.clone(),
                    ));
                }
            });

            Self {
                url,
                requests,
                interrupted,
                shutdown,
                task,
            }
        }
    }

    impl Drop for MediaOrigin {
        fn drop(&mut self) {
            self.shutdown.cancel();
            self.task.abort();
        }
    }

    async fn serve_media(
        stream: TcpStream,
        body: Arc<Vec<u8>>,
        declared_length: usize,
        initial_delay: Duration,
        requests: Arc<AtomicUsize>,
        interrupted: Arc<AtomicUsize>,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut request = String::new();
        if reader.read_line(&mut request).await.unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
        }

        requests.fetch_add(1, Ordering::SeqCst);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            declared_length
        );
        if write.write_all(header.as_bytes()).await.is_err() {
            return;
        }

        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(initial_delay) => {}
        }

        for chunk in body.chunks(8 * 1024) {
            if shutdown.is_cancelled() {
                return;
            }
            if write.write_all(chunk).await.is_err() {
                interrupted.fetch_add(1, Ordering::SeqCst);
                return;
            }

            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    }

    async fn server_for(host: &MockHost, host_limits: Value) -> RunningServer {
        let mut config = host_config(
            "meta-host",
            host,
            ops(&[("example.metadata", "media.resolve", true)]),
            host_limits,
        );
        config["allowExternalMedia"] = json!(true);

        RunningServer::start_configured(2, 2, false, json!([config]), json!({})).await
    }

    async fn server_for_media_origin(
        host: &MockHost,
        host_limits: Value,
        media_url: &str,
    ) -> RunningServer {
        let mut config = host_config(
            "meta-host",
            host,
            ops(&[("example.metadata", "media.resolve", true)]),
            host_limits,
        );
        let media_origin = url::Url::parse(media_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        config["mediaOrigins"] = json!([media_origin]);
        config["allowExternalMedia"] = json!(true);

        RunningServer::start_configured(2, 2, false, json!([config]), json!({})).await
    }

    async fn play_operation(socket: &mut TestSocket, origin: &Origin, input: Value) {
        let created = rpc(
            socket,
            "guild",
            "guild.create",
            json!({"guildId": GUILD, "enableFallbackSeeking": true}),
        )
        .await;
        assert_eq!(created["ok"], true, "{created}");
        let mut input = input;
        input["url"] = json!(origin.url);
        let played = rpc(
            socket,
            "play",
            "player.play",
            json!({"guildId": GUILD, "source": {
                "type": "operation", "hostId": "meta-host", "pluginId": "example.metadata",
                "operation": "media.resolve", "input": input,
            }}),
        )
        .await;
        assert_eq!(played["ok"], true, "{played}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = rpc(socket, "wait", "player.state", json!({"guildId": GUILD})).await;
            if state["result"]["position"]["media"]["positionMs"]
                .as_u64()
                .unwrap_or(0)
                > 3_000
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "playback never advanced: {state}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn delayed_plugin_media_is_cancelled_and_releases_stream_resources_repeatedly() {
        let host = MockHost::start(&[("example.metadata", "media.resolve")]).await;
        let origin = MediaOrigin::start(silent_wave(200)).await;
        let server = server_for_media_origin(&host, json!({}), &origin.url).await;
        let mut socket = session(&server, BOT_A).await;
        let created = rpc(
            &mut socket,
            "guild",
            "guild.create",
            json!({"guildId": GUILD}),
        )
        .await;
        assert_eq!(created["ok"], true, "{created}");

        for cycle in 1..=3 {
            let played = rpc(
                &mut socket,
                "play",
                "player.play",
                json!({"guildId": GUILD, "source": {
                    "type": "operation",
                    "hostId": "meta-host",
                    "pluginId": "example.metadata",
                    "operation": "media.resolve",
                    "input": {"mode": "media-url", "url": origin.url}
                }}),
            )
            .await;
            assert_eq!(played["ok"], true, "cycle {cycle}: {played}");

            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let state = rpc(
                    &mut socket,
                    "state",
                    "player.state",
                    json!({"guildId": GUILD}),
                )
                .await;
                if state["result"]["position"]["media"]["positionMs"]
                    .as_u64()
                    .unwrap_or(0)
                    > 3_000
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cycle {cycle} never decoded audio: {state}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let stats = plugin_stats(&mut socket, "meta-host").await;
            assert_eq!(stats["totals"]["grantsOpened"], cycle);
            assert!(stats["totals"]["mediaBytes"].as_u64().unwrap() > 0);

            let stop_started = Instant::now();
            let stopped = rpc(
                &mut socket,
                "stop",
                "player.stop",
                json!({"guildId": GUILD}),
            )
            .await;
            assert_eq!(stopped["ok"], true, "cycle {cycle}: {stopped}");

            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let stats = plugin_stats(&mut socket, "meta-host").await;
                    if stats["resources"]["activeStreams"] == 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle} left an active media transfer"));
            assert!(
                stop_started.elapsed() < Duration::from_secs(2),
                "cycle {cycle} took too long to release its media transfer"
            );

            tokio::time::timeout(Duration::from_secs(5), async {
                while origin.interrupted.load(Ordering::SeqCst) < cycle as usize {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle} did not close its delayed upstream"));
        }

        assert_eq!(origin.requests.load(Ordering::SeqCst), 3);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn truncated_plugin_media_response_fails_and_releases_its_stream_slot() {
        let host = MockHost::start(&[("example.metadata", "media.resolve")]).await;
        let mut body = silent_wave(200);
        let declared_length = body.len();
        body.truncate(44 + 1024 * 1024);
        let origin = MediaOrigin::start_with_declared_length(body, declared_length).await;
        let server = server_for_media_origin(&host, json!({}), &origin.url).await;
        let mut socket = session(&server, BOT_A).await;
        let created = rpc(
            &mut socket,
            "guild",
            "guild.create",
            json!({"guildId": GUILD}),
        )
        .await;
        assert_eq!(created["ok"], true, "{created}");

        let played = rpc(
            &mut socket,
            "play",
            "player.play",
            json!({"guildId": GUILD, "source": {
                "type": "operation",
                "hostId": "meta-host",
                "pluginId": "example.metadata",
                "operation": "media.resolve",
                "input": {"mode": "media-url", "url": origin.url}
            }}),
        )
        .await;
        assert_eq!(played["ok"], true, "{played}");

        let failed = tokio::time::timeout(
            Duration::from_secs(10),
            next_event(&mut socket, "player.failed"),
        )
        .await
        .expect("the truncated response must fail playback");
        assert_eq!(failed["data"]["generation"], played["result"]["generation"]);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let stats = plugin_stats(&mut socket, "meta-host").await;
                if stats["resources"]["activeStreams"] == 0 {
                    assert_eq!(stats["totals"]["mediaFailures"], 1);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a malformed response must release its active-stream slot");
        assert_eq!(origin.requests.load(Ordering::SeqCst), 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn stalled_plugin_media_times_out_and_releases_its_upstream() {
        let host = MockHost::start(&[("example.metadata", "media.resolve")]).await;
        let origin = MediaOrigin::start_delayed(silent_wave(20), Duration::from_millis(500)).await;
        let server =
            server_for_media_origin(&host, json!({"mediaIdleTimeoutMs": 100}), &origin.url).await;
        let mut socket = session(&server, BOT_A).await;
        let created = rpc(
            &mut socket,
            "guild",
            "guild.create",
            json!({"guildId": GUILD}),
        )
        .await;
        assert_eq!(created["ok"], true, "{created}");

        let played = rpc(
            &mut socket,
            "play",
            "player.play",
            json!({"guildId": GUILD, "source": {
                "type": "operation",
                "hostId": "meta-host",
                "pluginId": "example.metadata",
                "operation": "media.resolve",
                "input": {"mode": "media-url", "url": origin.url}
            }}),
        )
        .await;
        assert_eq!(played["ok"], false, "{played}");
        assert_eq!(played["error"]["code"], "playback_failed", "{played}");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let stats = plugin_stats(&mut socket, "meta-host").await;
                if stats["resources"]["activeStreams"] == 0 {
                    assert_eq!(stats["totals"]["mediaFailures"], 1);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stalled media must release its active-stream slot promptly");

        tokio::time::timeout(Duration::from_secs(2), async {
            while origin.interrupted.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stalled media failure must close the upstream response");

        assert_eq!(origin.requests.load(Ordering::SeqCst), 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_seek_runs_the_same_operation_again_as_the_same_client() {
        let host = MockHost::start(&[("example.metadata", "media.resolve")]).await;
        let server = server_for(&host, json!({})).await;
        let origin = Origin::start(silent_wave(200));
        let mut socket = session(&server, BOT_A).await;
        play_operation(&mut socket, &origin, json!({"mode": "media-url"})).await;
        assert_eq!(host.count(&host.state.execute_calls), 1);

        let seek = rpc(
            &mut socket,
            "seek",
            "player.seek",
            json!({"guildId": GUILD, "positionMs": 500}),
        )
        .await;

        assert_eq!(seek["ok"], true, "{seek}");
        assert_eq!(seek["result"]["fallback"], true);
        assert_eq!(origin.connections(), 2);
        // The operation ran a second time, with the input and credentials of the first, under
        // its own operation id.
        assert_eq!(host.count(&host.state.execute_calls), 2);
        let inputs = host.state.inputs.lock().unwrap().clone();
        assert_eq!(inputs[0], inputs[1]);
        let authorization = host.state.authorization.lock().unwrap().clone();
        assert_eq!(authorization[0], format!("Bearer {TOKEN}"));
        assert_eq!(authorization[0], authorization[1]);
        let ids = host.state.operation_ids.lock().unwrap().clone();
        assert!(
            !ids[0].is_empty() && !ids[1].is_empty() && ids[0] != ids[1],
            "{ids:?}"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_a_seek_cancels_the_operation_it_is_waiting_on() {
        let host = MockHost::start(&[("example.metadata", "media.resolve")]).await;
        let server = server_for(&host, json!({})).await;
        let origin = Origin::start(silent_wave(200));
        let mut socket = session(&server, BOT_A).await;
        // The first call answers; the reopening call never does.
        play_operation(
            &mut socket,
            &origin,
            json!({"mode": "media-url", "hangAfter": 1}),
        )
        .await;

        send(
            &mut socket,
            "seek",
            "player.seek",
            json!({"guildId": GUILD, "positionMs": 500}),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.count(&host.state.in_flight) == 0 {
            assert!(Instant::now() < deadline, "the operation was never re-run");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        send(
            &mut socket,
            "cancel",
            "player.seek.cancel",
            json!({"guildId": GUILD}),
        )
        .await;
        let answers = receive_many(&mut socket, &["cancel", "seek"]).await;
        assert_eq!(
            answers["cancel"]["result"]["cancelled"], true,
            "{answers:?}"
        );
        assert_eq!(answers["seek"]["error"]["code"], "cancelled", "{answers:?}");

        // The host saw its request abandoned, and its permit came back.
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.count(&host.state.closed_early) == 0 {
            assert!(
                Instant::now() < deadline,
                "the host request was not cancelled"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let stats = wait_for_idle(&mut socket, "meta-host").await;
        assert_eq!(stats["resources"]["pending"], 0);
        let state = rpc(
            &mut socket,
            "after",
            "player.state",
            json!({"guildId": GUILD}),
        )
        .await;
        assert_eq!(state["result"]["state"], "playing");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn the_hosts_limits_apply_to_the_reopening_and_a_refusal_leaves_playback_alone() {
        let host = MockHost::start(&[("example.metadata", "media.resolve")]).await;
        let server = server_for(&host, json!({"maxPendingOperations": 1})).await;
        let origin = Origin::start(silent_wave(200));
        let mut socket = session(&server, BOT_A).await;
        play_operation(&mut socket, &origin, json!({"mode": "media-url"})).await;

        // Another client of the host occupies its only slot.
        send(
            &mut socket,
            "hog",
            "plugin.execute",
            execute_params("meta-host", "media.resolve", json!({"mode": "hang"})),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.count(&host.state.in_flight) == 0 {
            assert!(Instant::now() < deadline, "the host was never busy");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let calls = host.count(&host.state.execute_calls);

        let seek = rpc(
            &mut socket,
            "seek",
            "player.seek",
            json!({"guildId": GUILD, "positionMs": 500}),
        )
        .await;

        assert_eq!(seek["ok"], false, "{seek}");
        assert_eq!(
            host.count(&host.state.execute_calls),
            calls,
            "a refused reopening never reached the host"
        );
        let state = rpc(
            &mut socket,
            "after",
            "player.state",
            json!({"guildId": GUILD}),
        )
        .await;
        assert_eq!(state["result"]["state"], "playing");
        assert!(
            state["result"]["position"]["media"]["positionMs"]
                .as_u64()
                .unwrap()
                > 3_000,
            "{state}"
        );

        server.shutdown().await;
    }
}
