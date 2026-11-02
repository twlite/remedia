use std::error::Error;
use std::sync::Arc;

use remedia_core::{InputSource, OpusEncoderOutput, PlaybackSession, PlaybackState};
use remedia_discord::{DiscordVoiceConnection, VoiceConnectionInfo};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let info = VoiceConnectionInfo {
        guild_id: env_u64("DISCORD_GUILD_ID")?,
        channel_id: env_u64("DISCORD_CHANNEL_ID")?,
        user_id: env_u64("DISCORD_USER_ID")?,
        session_id: required_env("DISCORD_VOICE_SESSION_ID")?,
        token: required_env("DISCORD_VOICE_TOKEN")?,
        endpoint: required_env("DISCORD_VOICE_ENDPOINT")?,
    };
    let source_value = required_env("REMEDIA_SOURCE")?;
    let source = match required_env("REMEDIA_SOURCE_TYPE")?.as_str() {
        "file" => InputSource::file(source_value),
        "url" => InputSource::http_url(source_value),
        other => {
            return Err(format!("REMEDIA_SOURCE_TYPE must be `file` or `url`, got {other}").into());
        }
    };

    let connection = DiscordVoiceConnection::connect(info).await?;
    println!("voice connection state: {:?}", connection.state());

    let session = Arc::new(PlaybackSession::new());
    let output = OpusEncoderOutput::new(connection.opus_sender())?;
    let starting_session = Arc::clone(&session);
    let metadata =
        tokio::task::spawn_blocking(move || starting_session.play(source, output)).await??;
    println!(
        "playing codec={} sample_rate={:?} channels={:?} duration={:?}",
        metadata.codec, metadata.sample_rate, metadata.channels, metadata.duration
    );

    let waiting_session = Arc::clone(&session);
    let state = tokio::task::spawn_blocking(move || waiting_session.wait()).await?;
    println!(
        "playback ended in state {state:?}; position {:?}",
        session.position()
    );
    if state == PlaybackState::Failed {
        eprintln!("playback error: {:?}", session.last_error());
    }

    session.stop()?;
    connection.disconnect().await?;
    Ok(())
}

fn required_env(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name)
        .map_err(|_| format!("required environment variable {name} is missing").into())
}

fn env_u64(name: &str) -> Result<u64, Box<dyn Error>> {
    Ok(required_env(name)?.parse()?)
}
