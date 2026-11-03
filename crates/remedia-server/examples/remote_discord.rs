//! Manual WebSocket RPC exercise for a running Remedia server and externally collected Discord
//! voice credentials. This example does not connect to Discord Gateway or send OP 4.

use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderName};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ExampleResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[tokio::main]
async fn main() -> ExampleResult<()> {
    let url = env_or("REMEDIA_SERVER_URL", "ws://127.0.0.1:8787/v1/ws");
    let username = required("REMEDIA_USERNAME")?;
    let password = required("REMEDIA_PASSWORD")?;
    let bot_user_id = required("REMEDIA_BOT_USER_ID")?;
    let guild_id = required("REMEDIA_GUILD_ID")?;
    let channel_id = required("REMEDIA_CHANNEL_ID")?;
    let media_url = required("REMEDIA_MEDIA_URL")?;
    let voice_session_id = required("REMEDIA_VOICE_SESSION_ID")?;
    let voice_token = required("REMEDIA_VOICE_TOKEN")?;
    let voice_endpoint = required("REMEDIA_VOICE_ENDPOINT")?;

    let mut request = url.into_client_request()?;
    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Basic {credentials}").parse()?);
    request.headers_mut().insert(
        HeaderName::from_static("x-remedia-bot-id"),
        bot_user_id.parse()?,
    );
    let (mut socket, _) = connect_async(request).await?;
    let mut client_session_id = None::<String>;
    let mut guild_created = false;
    let mut voice_connected = false;

    let result = async {
        let created = rpc(&mut socket, "client.create", json!({})).await?;
        let session_id = created["sessionId"]
            .as_str()
            .ok_or_else(|| error("client.create returned no sessionId"))?
            .to_owned();
        client_session_id = Some(session_id);

        rpc(&mut socket, "guild.create", json!({"guildId": guild_id})).await?;
        guild_created = true;
        rpc(
            &mut socket,
            "voice.connect",
            json!({
                "guildId": guild_id,
                "channelId": channel_id,
                "sessionId": voice_session_id,
                "token": voice_token,
                "endpoint": voice_endpoint
            }),
        )
        .await?;
        voice_connected = true;

        rpc(
            &mut socket,
            "player.play",
            json!({"guildId": guild_id, "source": {"type": "url", "url": media_url}}),
        )
        .await?;
        println!("Playback started; applying live filter changes.");
        rpc(
            &mut socket,
            "player.filter.set",
            json!({"guildId": guild_id, "graph": "bass=g=6"}),
        )
        .await?;
        rpc(
            &mut socket,
            "player.filter.set",
            json!({"guildId": guild_id, "graph": "tremolo=f=5:d=0.5"}),
        )
        .await?;
        rpc(
            &mut socket,
            "player.filter.clear",
            json!({"guildId": guild_id}),
        )
        .await?;
        let state = rpc(&mut socket, "player.state", json!({"guildId": guild_id})).await?;
        println!("Current playback state: {}", state["state"]);
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;

    if guild_created {
        let _ = rpc(&mut socket, "player.stop", json!({"guildId": guild_id})).await;
        if voice_connected {
            let _ = rpc(
                &mut socket,
                "voice.disconnect",
                json!({"guildId": guild_id}),
            )
            .await;
        }
        let _ = rpc(&mut socket, "guild.destroy", json!({"guildId": guild_id})).await;
    }
    if client_session_id.is_some() {
        let _ = rpc(&mut socket, "client.close", json!({})).await;
    }
    let _ = socket.close(None).await;
    result
}

async fn rpc(socket: &mut Socket, command: &str, params: Value) -> ExampleResult<Value> {
    let id = format!("manual-{}", REQUEST_ID.fetch_add(1, Ordering::Relaxed));
    socket
        .send(Message::Text(
            json!({"version": 1, "id": id, "command": command, "params": params})
                .to_string()
                .into(),
        ))
        .await?;
    loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| error("server closed the WebSocket"))??;
        let Message::Text(text) = message else {
            continue;
        };
        let response: Value = serde_json::from_str(text.as_str())?;
        if response.get("id").and_then(Value::as_str) != Some(&id) {
            // Lifecycle event frames are intentionally independent of request responses.
            continue;
        }
        if response["ok"] == true {
            return Ok(response["result"].clone());
        }
        return Err(error(format!(
            "RPC {command} failed: {} ({})",
            response["error"]["message"]
                .as_str()
                .unwrap_or("unknown error"),
            response["error"]["code"].as_str().unwrap_or("unknown")
        )));
    }
}

fn required(name: &str) -> ExampleResult<String> {
    std::env::var(name).map_err(|_| error(format!("set required environment variable {name}")))
}

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

fn error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::other(message.into()))
}
