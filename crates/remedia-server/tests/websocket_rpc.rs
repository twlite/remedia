use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite};

use remedia_server::{ServerConfig, ServerHandle};

static NEXT_CONFIG: AtomicU64 = AtomicU64::new(1);
const BOT_A_ID: &str = "100000000000000001";
const BOT_B_ID: &str = "100000000000000002";

struct RunningServer {
    server: ServerHandle,
    config_dir: PathBuf,
}

impl RunningServer {
    async fn start(resume_seconds: u64, max_players_per_client: usize) -> Self {
        Self::start_with_media_policy(resume_seconds, max_players_per_client, true).await
    }

    async fn start_with_media_policy(
        resume_seconds: u64,
        max_players_per_client: usize,
        block_private_networks: bool,
    ) -> Self {
        let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
        let config_dir =
            std::env::temp_dir().join(format!("remedia-server-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&config_dir).unwrap();
        let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../remedia-core/tests/fixtures")
            .canonicalize()
            .unwrap();
        let json = json!({
            "bind": {"host": "127.0.0.1", "port": 0},
            "users": [
                {"username": "bot-a", "password": "password-a"},
                {"username": "bot-b", "password": "password-b"}
            ],
            "sessionResumptionTimeoutSeconds": resume_seconds,
            "limits": {
                "maxConnections": 16,
                "maxClientSessions": 8,
                "maxTotalPlayers": 8,
                "maxPlayersPerClient": max_players_per_client,
                "maxMessageBytes": 65536,
                "requestTimeoutMs": 5000,
                "maxInFlightRequests": 8,
                "maxRequestsPerSecond": 60,
                "eventBuffer": 8
            },
            "media": {
                "allowedFileRoots": [fixture_root],
                "allowedUrlHosts": ["*"],
                "allowHttp": true,
                "allowHttps": true,
                "blockPrivateNetworks": block_private_networks
            },
            "audio": {"opus": {"bitrate": 96000, "complexity": 5, "vbr": false}},
            "observability": {"console": false}
        });
        let config_path = config_dir.join("remedia.config.json");
        std::fs::write(&config_path, json.to_string()).unwrap();
        let config = ServerConfig::from_file(&config_path).unwrap();
        let server = ServerHandle::start(config).await.unwrap();
        Self { server, config_dir }
    }

    fn url(&self) -> String {
        format!("ws://{}/v1/ws", self.server.local_addr())
    }

    async fn connect(&self, username: &str, password: &str) -> Result<TestSocket, WsError> {
        self.connect_as(username, password, BOT_A_ID).await
    }

    async fn connect_as(
        &self,
        username: &str,
        password: &str,
        bot_user_id: &str,
    ) -> Result<TestSocket, WsError> {
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let mut request = self.url().into_client_request().unwrap();
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Basic {credentials}").parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-remedia-bot-id", bot_user_id.parse().unwrap());
        let (socket, _) = connect_async(request).await?;
        Ok(socket)
    }

    async fn connect_without_bot_id(
        &self,
        username: &str,
        password: &str,
    ) -> Result<TestSocket, WsError> {
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let mut request = self.url().into_client_request().unwrap();
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Basic {credentials}").parse().unwrap(),
        );
        let (socket, _) = connect_async(request).await?;
        Ok(socket)
    }

    async fn shutdown(self) {
        self.server.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(self.config_dir);
    }
}

type TestSocket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn rpc(socket: &mut TestSocket, id: &str, command: &str, params: Value) -> Value {
    socket
        .send(Message::Text(
            json!({"version": 1, "id": id, "command": command, "params": params})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(text.as_str()).unwrap();
        if value.get("id").and_then(Value::as_str) == Some(id) {
            return value;
        }
    }
}

async fn rpc_collect_events(
    socket: &mut TestSocket,
    id: &str,
    command: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    socket
        .send(Message::Text(
            json!({"version": 1, "id": id, "command": command, "params": params})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(text.as_str()).unwrap();
        if value.get("id").and_then(Value::as_str) == Some(id) {
            return (value, events);
        }
        if value.get("event").is_some() {
            events.push(value);
        }
    }
}

async fn next_event(socket: &mut TestSocket, name: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value.get("event").and_then(Value::as_str) == Some(name) {
                return value;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for lifecycle event {name}"))
}

async fn create_client(socket: &mut TestSocket) -> (String, String) {
    let response = rpc(socket, "client-create", "client.create", json!({})).await;
    assert_eq!(response["ok"], true, "{response}");
    (
        response["result"]["sessionId"].as_str().unwrap().to_owned(),
        response["result"]["resumeCredential"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

#[path = "websocket_rpc/authentication.rs"]
mod authentication;
#[path = "websocket_rpc/ownership.rs"]
mod ownership;
#[path = "websocket_rpc/playback.rs"]
mod playback;
#[path = "websocket_rpc/probe.rs"]
mod probe;
#[path = "websocket_rpc/sessions.rs"]
mod sessions;
