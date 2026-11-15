//! Seeking a source that cannot seek, through the server: the applied position, cancellation,
//! and the guild staying responsive while a replacement is prepared.

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Instant;

const GUILD: &str = "300000000000000001";
const OTHER_GUILD: &str = "300000000000000002";

/// A mono 48 kHz WAV of `seconds`, silent. Pacing at 8 KiB per 5 ms plays it about 17 times
/// faster than real time, which keeps a track alive for several seconds.
pub(super) fn silent_wave(seconds: usize) -> Vec<u8> {
    let samples = seconds * 48_000;
    let mut bytes = Vec::with_capacity(44 + samples * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((36 + samples * 2) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&((samples * 2) as u32).to_le_bytes());
    bytes.resize(44 + samples * 2, 0);
    bytes
}

/// An origin that ignores `Range`. The first connection is paced, so a track plays for a while
/// even though the server's output does not pace it; later connections answer at full speed once
/// `hold` is released.
pub(super) struct Origin {
    pub(super) url: String,
    connections: Arc<AtomicUsize>,
    pub(super) hold: Arc<AtomicBool>,
}

impl Origin {
    pub(super) fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/track.wav", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(AtomicBool::new(false));
        let (shared_connections, shared_hold) = (Arc::clone(&connections), Arc::clone(&hold));

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let body = body.clone();
                let (connections, hold) =
                    (Arc::clone(&shared_connections), Arc::clone(&shared_hold));
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                            break;
                        }
                    }
                    let index = connections.fetch_add(1, Ordering::SeqCst);
                    while index > 0 && hold.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(header.as_bytes()).is_err() {
                        return;
                    }
                    for chunk in body.chunks(8 * 1024) {
                        if stream.write_all(chunk).is_err() {
                            return;
                        }
                        if index == 0 {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                });
            }
        });

        Self {
            url,
            connections,
            hold,
        }
    }

    pub(super) fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    async fn wait_for_connections(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.connections() < count {
            assert!(Instant::now() < deadline, "the origin was never contacted");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
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

/// Read until the response to `id`, collecting events that arrive first.
async fn response(socket: &mut TestSocket, id: &str, events: &mut Vec<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value.get("id").and_then(Value::as_str) == Some(id) {
                return value;
            }
            if value.get("event").is_some() {
                events.push(value);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no response to {id}"))
}

/// A guild playing `origin`, once its media position is past `after_ms`.
async fn playing(
    server: &RunningServer,
    origin: &Origin,
    guild: &str,
    fallback: bool,
    after_ms: u64,
) -> TestSocket {
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    create_client(&mut socket).await;
    let created = rpc(
        &mut socket,
        "guild",
        "guild.create",
        json!({"guildId": guild, "enableFallbackSeeking": fallback}),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    let played = rpc(
        &mut socket,
        "play",
        "player.play",
        json!({"guildId": guild, "source": {"type": "url", "url": origin.url}}),
    )
    .await;
    assert_eq!(played["ok"], true, "{played}");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = rpc(
            &mut socket,
            "wait",
            "player.state",
            json!({"guildId": guild}),
        )
        .await;
        let position = state["result"]["position"]["media"]["positionMs"]
            .as_u64()
            .unwrap_or(0);
        if position > after_ms {
            return socket;
        }
        assert!(
            Instant::now() < deadline,
            "playback never got to {after_ms} ms: {state}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_fallback_seek_reports_where_it_landed_in_the_response_and_the_event() {
    let server = RunningServer::start_with_media_policy(60, 4, false).await;
    let origin = Origin::start(silent_wave(200));
    let mut socket = playing(&server, &origin, GUILD, true, 3_000).await;

    let mut events = Vec::new();
    send(
        &mut socket,
        "seek",
        "player.seek",
        json!({"guildId": GUILD, "positionMs": 500}),
    )
    .await;
    let result = response(&mut socket, "seek", &mut events).await;

    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["result"]["fallback"], true);
    assert_eq!(result["result"]["requestedPositionMs"], 500);
    let applied = result["result"]["appliedPositionMs"].as_u64().unwrap();
    // The old source kept playing while the second one was prepared.
    assert!(applied >= 500, "{applied}");
    assert_eq!(origin.connections(), 2);

    let event = if let Some(event) = events.iter().find(|e| e["event"] == "player.seeked") {
        event.clone()
    } else {
        next_event(&mut socket, "player.seeked").await
    };
    assert_eq!(event["data"]["positionMs"], applied);
    assert_eq!(event["data"]["requestedPositionMs"], 500);
    assert_eq!(event["data"]["fallback"], true);
    assert!(event["data"]["previousPositionMs"].as_u64().unwrap() > 3_000);

    server.shutdown().await;
}

#[tokio::test]
async fn a_seek_that_outlives_its_deadline_is_cancelled_and_never_applies() {
    let server =
        RunningServer::start_configured(60, 4, false, json!([]), json!({"requestTimeoutMs": 4000}))
            .await;
    let origin = Origin::start(silent_wave(200));
    let mut socket = playing(&server, &origin, GUILD, true, 3_000).await;
    origin.hold.store(true, Ordering::SeqCst);

    let mut events = Vec::new();
    send(
        &mut socket,
        "seek",
        "player.seek",
        json!({"guildId": GUILD, "positionMs": 500}),
    )
    .await;
    let result = response(&mut socket, "seek", &mut events).await;
    assert_eq!(result["error"]["code"], "request_timeout", "{result}");

    // Letting the origin answer now must not complete a seek the client was told had failed.
    origin.hold.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let state = rpc(
        &mut socket,
        "state",
        "player.state",
        json!({"guildId": GUILD}),
    )
    .await;
    let position = state["result"]["position"]["media"]["positionMs"]
        .as_u64()
        .unwrap();
    assert!(
        position > 3_000,
        "the cancelled seek moved playback to {position}: {state}"
    );
    assert!(
        !events.iter().any(|event| event["event"] == "player.seeked"),
        "no seek was applied: {events:?}"
    );
    let late = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["event"] == "player.seeked" {
                return value;
            }
        }
    })
    .await;
    assert!(
        late.is_err(),
        "a seek was applied after the timeout: {late:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn the_guild_stays_responsive_while_a_seek_prepares_and_it_can_be_cancelled() {
    let server = RunningServer::start_with_media_policy(60, 4, false).await;
    let origin = Origin::start(silent_wave(200));
    let mut socket = playing(&server, &origin, GUILD, true, 3_000).await;
    // A second guild on the same connection, unrelated to the first.
    let other = rpc(
        &mut socket,
        "other",
        "guild.create",
        json!({"guildId": OTHER_GUILD}),
    )
    .await;
    assert_eq!(other["ok"], true, "{other}");

    origin.hold.store(true, Ordering::SeqCst);
    let mut events = Vec::new();
    send(
        &mut socket,
        "seek",
        "player.seek",
        json!({"guildId": GUILD, "positionMs": 500}),
    )
    .await;
    origin.wait_for_connections(2).await;

    // Commands for the same guild and for another one are answered while the seek waits.
    let started = Instant::now();
    for (id, command, params) in [
        (
            "volume",
            "player.volume.set",
            json!({"guildId": GUILD, "gain": 0.5}),
        ),
        ("state", "player.state", json!({"guildId": GUILD})),
        (
            "other-state",
            "player.state",
            json!({"guildId": OTHER_GUILD}),
        ),
    ] {
        send(&mut socket, id, command, params).await;
        let answer = response(&mut socket, id, &mut events).await;
        assert_eq!(answer["ok"], true, "{id}: {answer}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // Cancelling reaches the waiting seek even though it holds nothing the cancel needs.
    send(
        &mut socket,
        "cancel",
        "player.seek.cancel",
        json!({"guildId": GUILD}),
    )
    .await;
    let cancel = response(&mut socket, "cancel", &mut events).await;
    assert_eq!(cancel["result"]["cancelled"], true, "{cancel}");
    let seek = response(&mut socket, "seek", &mut events).await;
    assert_eq!(seek["error"]["code"], "cancelled", "{seek}");

    // A second cancel finds nothing, and playback was never interrupted.
    let again = rpc(
        &mut socket,
        "again",
        "player.seek.cancel",
        json!({"guildId": GUILD}),
    )
    .await;
    assert_eq!(again["result"]["cancelled"], false);
    let state = rpc(
        &mut socket,
        "after",
        "player.state",
        json!({"guildId": GUILD}),
    )
    .await;
    assert_eq!(state["result"]["state"], "playing");
    origin.hold.store(false, Ordering::SeqCst);

    server.shutdown().await;
}
