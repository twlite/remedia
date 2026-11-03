use std::sync::Arc;
use std::sync::atomic::Ordering;

use napi::bindgen_prelude::Status;
use napi_derive::napi;
use remedia_core::PlaybackSession;
use remedia_discord::{
    ConnectionState, DiscordOpusSenderRouter, DiscordVoiceConnection, VoiceConnectionInfo,
};

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
        let sender_router = DiscordOpusSenderRouter::new();
        sender_router.attach(connection.opus_sender());
        let entry = PlaybackEntry::new(
            PlaybackSession::with_observability(self.inner.observability_handle.clone()),
            Some(voice_id.clone()),
            Some(sender_router.clone()),
        );
        lock(&self.inner.playbacks).insert(playback_id.clone(), entry);
        lock(&self.inner.voices).insert(
            voice_id.clone(),
            VoiceEntry {
                connection,
                playback_id: playback_id.clone(),
                sender_router,
                transport_operations: Arc::new(napi::tokio::sync::Mutex::new(())),
                transport_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        );

        Ok(NativeVoiceSessionIds {
            voice_id,
            playback_id,
        })
    }

    /// Replace only the Discord transport while retaining the existing
    /// playback decoder, filters, volume, pause state, and output timeline.
    #[napi]
    pub async fn replace_voice(
        &self,
        voice_id: String,
        generation: u32,
        options: NativeVoiceOptions,
    ) -> napi::Result<()> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let voice = self.inner.voice(&voice_id)?;
        if voice.transport_generation.load(Ordering::Acquire) != u64::from(generation) {
            return Err(napi_error(
                Status::GenericFailure,
                "Discord voice recovery attempt was superseded",
            ));
        }
        let _transport_operation = voice.transport_operations.lock().await;
        if voice.transport_generation.load(Ordering::Acquire) != u64::from(generation) {
            return Err(napi_error(
                Status::GenericFailure,
                "Discord voice recovery attempt was superseded",
            ));
        }
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

        voice.sender_router.detach();
        voice.connection.disconnect().await.map_err(|error| {
            voice_error("could not close the previous Discord transport", error)
        })?;
        if voice.transport_generation.load(Ordering::Acquire) != u64::from(generation) {
            return Err(napi_error(
                Status::GenericFailure,
                "Discord voice recovery attempt was superseded",
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
        .map_err(|error| voice_error("Discord voice transport recovery failed", error))?;
        if self.inner.closed.load(Ordering::Acquire)
            || voice.transport_generation.load(Ordering::Acquire) != u64::from(generation)
        {
            let _ = connection.disconnect().await;
            if self.inner.closed.load(Ordering::Acquire) {
                voice
                    .sender_router
                    .fail("Remedia instance closed during voice recovery");
                return Err(napi_error(
                    Status::GenericFailure,
                    "Remedia instance is closed",
                ));
            }
            return Err(napi_error(
                Status::GenericFailure,
                "Discord voice recovery attempt was superseded",
            ));
        }

        voice.sender_router.attach(connection.opus_sender());
        lock(&self.inner.voices).insert(
            voice_id,
            VoiceEntry {
                connection,
                playback_id: voice.playback_id.clone(),
                sender_router: voice.sender_router.clone(),
                transport_operations: Arc::clone(&voice.transport_operations),
                transport_generation: Arc::clone(&voice.transport_generation),
            },
        );
        Ok(())
    }

    /// Invalidate an in-flight transport replacement before newer Gateway
    /// credentials are used. This does not touch the active playback session.
    #[napi]
    pub fn invalidate_voice_recovery(&self, voice_id: String, generation: u32) -> napi::Result<()> {
        self.inner.ensure_open()?;
        let voice = self.inner.voice(&voice_id)?;
        voice
            .transport_generation
            .store(u64::from(generation), Ordering::Release);
        voice.sender_router.detach();
        Ok(())
    }

    /// Close the old Discord transport while leaving the playback session parked for recovery.
    #[napi]
    pub async fn suspend_voice(&self, voice_id: String, generation: u32) -> napi::Result<()> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let voice = self.inner.voice(&voice_id)?;
        if voice.transport_generation.load(Ordering::Acquire) != u64::from(generation) {
            return Ok(());
        }
        let _transport_operation = voice.transport_operations.lock().await;
        if voice.transport_generation.load(Ordering::Acquire) != u64::from(generation) {
            return Ok(());
        }
        voice
            .connection
            .disconnect()
            .await
            .map_err(|error| voice_error("could not suspend the old Discord transport", error))
    }

    /// Terminate a playback parked during voice recovery after retries fail.
    #[napi]
    pub fn fail_voice_recovery(&self, voice_id: String, message: String) -> napi::Result<()> {
        let voice = self.inner.voice(&voice_id)?;
        voice.transport_generation.fetch_add(1, Ordering::AcqRel);
        voice.sender_router.fail(message);
        Ok(())
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
        voice.transport_generation.fetch_add(1, Ordering::AcqRel);
        voice.sender_router.detach();
        let _transport_operation = voice.transport_operations.lock().await;
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
        voice
            .sender_router
            .fail("Discord voice session was released");
        lock(&self.inner.voices).remove(&voice_id);
        lock(&self.inner.playbacks).remove(&voice.playback_id);
        Ok(())
    }
}
