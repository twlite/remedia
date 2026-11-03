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

struct RunningServer {
    server: ServerHandle,
    config_dir: PathBuf,
}

impl RunningServer {
    async fn start(resume_seconds: u64, max_players_per_client: usize) -> Self {
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
                {"username": "bot-a", "password": "password-a", "discordBotUserId": "100000000000000001"},
                {"username": "bot-b", "password": "password-b", "discordBotUserId": "100000000000000002"}
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
                "blockPrivateNetworks": true
            },
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

#[tokio::test]
async fn websocket_auth_request_correlation_and_malformed_message_errors() {
    let server = RunningServer::start(60, 4).await;
    let unauthenticated = server.connect("bot-a", "wrong").await.unwrap_err();
    match unauthenticated {
        WsError::Http(response) => assert_eq!(response.status(), 401),
        error => panic!("expected a rejected handshake, received {error}"),
    }

    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    let response = rpc(&mut socket, "correlation-42", "node.info", json!({})).await;
    assert_eq!(response["id"], "correlation-42");
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["protocolVersion"], 1);

    socket.send(Message::Text("not-json".into())).await.unwrap();
    let malformed = socket.next().await.unwrap().unwrap();
    let Message::Text(malformed) = malformed else {
        panic!("expected a JSON error response")
    };
    let malformed: Value = serde_json::from_str(malformed.as_str()).unwrap();
    assert_eq!(malformed["id"], Value::Null);
    assert_eq!(malformed["error"]["code"], "invalid_request");

    let sessions = rpc(&mut socket, "create", "client.create", json!({})).await;
    assert_eq!(sessions["ok"], true);
    let bad_params = rpc(
        &mut socket,
        "bad-params",
        "node.info",
        json!({"unexpected": true}),
    )
    .await;
    assert_eq!(bad_params["error"]["code"], "invalid_request");
    drop(socket);
    server.shutdown().await;
}

#[tokio::test]
async fn client_and_guild_ownership_are_isolated_and_capacity_is_enforced() {
    let server = RunningServer::start(60, 1).await;
    let mut first = server.connect("bot-a", "password-a").await.unwrap();
    let (session_id, _) = create_client(&mut first).await;
    let created = rpc(
        &mut first,
        "guild-a",
        "guild.create",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    let limited = rpc(
        &mut first,
        "guild-limit",
        "guild.create",
        json!({"guildId": "200000000000000002"}),
    )
    .await;
    assert_eq!(limited["error"]["code"], "resource_exhausted");

    let mut second = server.connect("bot-b", "password-b").await.unwrap();
    let unauthorized_resume = rpc(
        &mut second,
        "resume-isolation",
        "client.resume",
        json!({"sessionId": session_id, "resumeCredential": "not-the-credential"}),
    )
    .await;
    assert_eq!(unauthorized_resume["error"]["code"], "unknown_session");
    let no_session = rpc(
        &mut second,
        "guild-isolation",
        "guild.get",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(no_session["error"]["code"], "unknown_session");

    let close = rpc(&mut first, "close-client", "client.close", json!({})).await;
    assert_eq!(close["ok"], true, "{close}");
    let stats = rpc(&mut second, "stats", "node.stats", json!({})).await;
    assert_eq!(stats["result"]["players"]["active"], 0);
    drop(first);
    drop(second);
    server.shutdown().await;
}

#[tokio::test]
async fn real_local_media_playback_and_cleanup_use_the_core_engine() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    create_client(&mut socket).await;
    let guild = "200000000000000010";
    assert_eq!(
        rpc(
            &mut socket,
            "guild",
            "guild.create",
            json!({"guildId": guild})
        )
        .await["ok"],
        true
    );

    let invalid_voice_endpoint = rpc(
        &mut socket,
        "voice-endpoint-policy",
        "voice.connect",
        json!({
            "guildId": "200000000000000010",
            "channelId": "200000000000000011",
            "sessionId": "fixture-session",
            "token": "fixture-token",
            "endpoint": "127.0.0.1:8787"
        }),
    )
    .await;
    assert_eq!(invalid_voice_endpoint["error"]["code"], "invalid_request");

    let invalid_pause = rpc(
        &mut socket,
        "pause-idle",
        "player.pause",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(invalid_pause["error"]["code"], "invalid_state");
    let filter = rpc(
        &mut socket,
        "set-filter",
        "player.filter.set",
        json!({"guildId": guild, "graph": "bass=g=6"}),
    )
    .await;
    assert_eq!(filter["ok"], true, "{filter}");
    let volume = rpc(
        &mut socket,
        "set-volume",
        "player.volume.set",
        json!({"guildId": guild, "gain": 0.75}),
    )
    .await;
    assert_eq!(volume["result"]["volume"], 0.75);
    let invalid_filter = rpc(
        &mut socket,
        "invalid-filter",
        "player.filter.set",
        json!({"guildId": guild, "graph": "not_a_real_filter"}),
    )
    .await;
    assert_eq!(invalid_filter["error"]["code"], "playback_failed");
    let configured_state = rpc(
        &mut socket,
        "configured-state",
        "player.state",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(configured_state["result"]["filterGraph"], "bass=g=6");
    assert_eq!(configured_state["result"]["volume"], 0.75);

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../remedia-core/tests/fixtures/generated-opus-passthrough.ogg")
        .canonicalize()
        .unwrap();
    let played = rpc(
        &mut socket,
        "play-local",
        "player.play",
        json!({"guildId": guild, "source": {"type":"file", "path": fixture}}),
    )
    .await;
    assert_eq!(played["ok"], true, "{played}");
    assert_eq!(played["result"]["codec"], "opus");
    let playback_generation = played["result"]["generation"].as_u64().unwrap();
    assert!(playback_generation > 0);
    assert!(played["result"]["durationMs"].as_u64().unwrap() > 0);

    let state = rpc(
        &mut socket,
        "playback-state",
        "player.state",
        json!({"guildId": guild}),
    )
    .await;
    assert!(matches!(
        state["result"]["state"].as_str(),
        Some("playing" | "completed")
    ));
    assert_eq!(
        state["result"]["generation"].as_u64(),
        Some(playback_generation)
    );
    assert_eq!(state["result"]["filterGraph"], "bass=g=6");
    assert_eq!(state["result"]["volume"], 0.75);
    let stopped = rpc(
        &mut socket,
        "stop",
        "player.stop",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    let clear_filter = rpc(
        &mut socket,
        "clear-filter",
        "player.filter.clear",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(clear_filter["ok"], true, "{clear_filter}");
    let current_volume = rpc(
        &mut socket,
        "current-volume",
        "player.state",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(current_volume["result"]["volume"], 0.75);

    let outside_root = rpc(
        &mut socket,
        "outside-file",
        "player.play",
        json!({"guildId": guild, "source": {"type":"file", "path": "/etc/hosts"}}),
    )
    .await;
    assert_eq!(outside_root["error"]["code"], "invalid_request");
    let destroyed = rpc(
        &mut socket,
        "destroy-guild",
        "guild.destroy",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(destroyed["ok"], true, "{destroyed}");
    let closed = rpc(&mut socket, "close", "client.close", json!({})).await;
    assert_eq!(closed["ok"], true, "{closed}");
    drop(socket);
    server.shutdown().await;
}

#[tokio::test]
async fn disconnected_client_can_resume_and_expired_sessions_are_removed() {
    let server = RunningServer::start(1, 4).await;
    let mut first = server.connect("bot-a", "password-a").await.unwrap();
    let (session_id, resume_credential) = create_client(&mut first).await;
    let guild = rpc(
        &mut first,
        "guild",
        "guild.create",
        json!({"guildId": "200000000000000020"}),
    )
    .await;
    assert_eq!(guild["ok"], true);
    first.send(Message::Close(None)).await.unwrap();
    drop(first);

    let mut reconnected = server.connect("bot-a", "password-a").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let listed = rpc(&mut reconnected, "list", "client.list", json!({})).await;
        if listed["result"]["sessions"][0]["attached"] == false {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket did not detach"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let resumed = rpc(
        &mut reconnected,
        "resume",
        "client.resume",
        json!({"sessionId": session_id, "resumeCredential": resume_credential}),
    )
    .await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    assert_eq!(
        resumed["result"]["resumeCredential"], resume_credential,
        "resume credentials remain stable if the resume response is lost"
    );
    assert_eq!(
        resumed["result"]["snapshot"]["guilds"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let stale = rpc(
        &mut reconnected,
        "stale-resume",
        "client.resume",
        json!({"sessionId": resumed["result"]["sessionId"], "resumeCredential": resume_credential}),
    )
    .await;
    assert_eq!(stale["error"]["code"], "invalid_state");
    rpc(&mut reconnected, "close", "client.close", json!({})).await;

    let mut expiring = server.connect("bot-a", "password-a").await.unwrap();
    let (expiring_id, _) = create_client(&mut expiring).await;
    expiring.send(Message::Close(None)).await.unwrap();
    drop(expiring);
    let mut observer = server.connect("bot-a", "password-a").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let listed = rpc(&mut observer, "list-expiry", "client.list", json!({})).await;
        let exists = listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["sessionId"] == expiring_id);
        if !exists {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached session did not expire"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(reconnected);
    drop(observer);
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_commands_and_invalid_ids_return_typed_errors() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    let unknown = rpc(&mut socket, "unknown", "node.magic", json!({})).await;
    assert_eq!(unknown["error"]["code"], "invalid_request");
    let invalid_id = rpc(
        &mut socket,
        "invalid-guild",
        "guild.create",
        json!({"guildId": "1.2"}),
    )
    .await;
    assert_eq!(invalid_id["error"]["code"], "invalid_request");
    drop(socket);
    server.shutdown().await;
}

#[tokio::test]
async fn server_shutdown_closes_all_remaining_sessions() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    create_client(&mut socket).await;
    rpc(
        &mut socket,
        "guild",
        "guild.create",
        json!({"guildId": "200000000000000030"}),
    )
    .await;
    server.shutdown().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("server should close the socket after sending queued events");
}

#[allow(dead_code)]
fn _assert_tungstenite_client_support_is_present(_: tungstenite::protocol::Role) {}
