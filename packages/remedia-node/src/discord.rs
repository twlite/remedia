use std::sync::Arc;
use std::sync::atomic::Ordering;

use napi::bindgen_prelude::Status;
use napi_derive::napi;
use remedia_core::PlaybackSession;
use remedia_discord::{ConnectionState, DiscordVoiceConnection, VoiceConnectionInfo};

use crate::native::NativeRemedia;
use crate::runtime::{PlaybackEntry, VoiceEntry, lock, run_blocking};
use crate::types::{NativeVoiceOptions, NativeVoiceSessionIds};
use crate::util::{
    connection_state_name, napi_error, parse_snowflake, playback_error, voice_error,
};

#[napi]
impl NativeRemedia {
    /// Open a Discord voice transport using session data collected by the caller's Gateway client.
    #[napi]
    pub async fn connect_voice(
        &self,
        options: NativeVoiceOptions,
    ) -> napi::Result<NativeVoiceSessionIds> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let guild_id = parse_snowflake("guildId", &options.guild_id)?;
        let channel_id = parse_snowflake("channelId", &options.channel_id)?;
        let user_id = parse_snowflake("userId", &options.user_id)?;
        if options.session_id.is_empty() || options.token.is_empty() || options.endpoint.is_empty()
        {
            return Err(napi_error(
                Status::InvalidArg,
                "sessionId, token, and endpoint must be non-empty",
            ));
        }

        let connection = DiscordVoiceConnection::connect_with_observability(
            VoiceConnectionInfo {
                guild_id,
                channel_id,
                user_id,
                session_id: options.session_id,
                token: options.token,
                endpoint: options.endpoint,
            },
            self.inner.observability_handle.clone(),
        )
        .await
        .map_err(|error| voice_error("Discord voice connection failed", error))?;

        if self.inner.closed.load(Ordering::Acquire) {
            let _ = connection.disconnect().await;
            return Err(napi_error(
                Status::GenericFailure,
                "Remedia instance is closed",
            ));
        }

        let voice_id = self.inner.id();
        let playback_id = self.inner.id();
        let entry = PlaybackEntry::new(
            PlaybackSession::with_observability(self.inner.observability_handle.clone()),
            Some(voice_id.clone()),
        );
        lock(&self.inner.playbacks).insert(playback_id.clone(), entry);
        lock(&self.inner.voices).insert(
            voice_id.clone(),
            VoiceEntry {
                connection,
                playback_id: playback_id.clone(),
            },
        );

        Ok(NativeVoiceSessionIds {
            voice_id,
            playback_id,
        })
    }

    /// Current transport state for one voice session.
    #[napi]
    pub fn voice_state(&self, voice_id: String) -> napi::Result<String> {
        let state = self.inner.voice(&voice_id)?.connection.state();
        Ok(connection_state_name(state).to_owned())
    }

    /// Return the latest transport failure for one voice session.
    #[napi]
    pub fn voice_error(&self, voice_id: String) -> napi::Result<Option<String>> {
        Ok(self
            .inner
            .voice(&voice_id)?
            .connection
            .failure()
            .map(|error| error.to_string()))
    }

    /// Stop playback and release the associated voice transport.
    #[napi]
    pub async fn disconnect_voice(&self, voice_id: String) -> napi::Result<()> {
        let _operation = self.inner.operations.read().await;
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let voice = self.inner.voice(&voice_id)?;
        let playback = self.inner.playback(&voice.playback_id)?;

        playback.cancel_start();
        let session = Arc::clone(&playback.session);
        let stop_result = run_blocking(move || session.stop()).await;
        let disconnect_result = voice.connection.disconnect().await;
        match stop_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(playback_error("could not stop voice playback", error));
            }
            Err(error) => return Err(error),
        }
        disconnect_result
            .map_err(|error| voice_error("could not disconnect voice transport", error))
    }

    /// Forget a voice session after its transport has been disconnected.
    #[napi]
    pub fn release_voice_session(&self, voice_id: String) -> napi::Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let voice = self.inner.voice(&voice_id)?;
        if !matches!(
            voice.connection.state(),
            ConnectionState::Disconnected | ConnectionState::Failed
        ) {
            return Err(napi_error(
                Status::GenericFailure,
                "disconnect the Discord voice transport before releasing its session",
            ));
        }
        lock(&self.inner.voices).remove(&voice_id);
        lock(&self.inner.playbacks).remove(&voice.playback_id);
        Ok(())
    }
}
