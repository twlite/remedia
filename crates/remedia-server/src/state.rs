use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rand::random;
use remedia_core::{CancellationToken, PlaybackSession, PlaybackState};
use remedia_discord::DiscordVoiceConnection;
use remedia_observability::{Observability, ObservabilityHandle};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::sync::watch;
use tokio::sync::{Semaphore, broadcast};
use tokio_util::sync::CancellationToken as TokioCancellationToken;
use tracing::instrument::WithSubscriber;

use crate::config::ServerConfig;
use crate::protocol::event;

pub(crate) struct ServerInner {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) observability: ObservabilityHandle,
    pub(crate) sessions: Mutex<HashMap<String, Arc<ClientSession>>>,
    pub(crate) active_players: AtomicUsize,
    pub(crate) active_connections: AtomicUsize,
    pub(crate) node_id: String,
    pub(crate) started_at: Instant,
    pub(crate) connection_slots: Arc<Semaphore>,
    pub(crate) observability_owner: Mutex<Option<Observability>>,
    pub(crate) shutdown: TokioCancellationToken,
}

pub(crate) struct ClientSession {
    pub(crate) id: String,
    pub(crate) principal: String,
    pub(crate) bot_user_id: u64,
    resume_credential: Mutex<String>,
    attached_connection: Mutex<Option<String>>,
    detached_deadline: Mutex<Option<Instant>>,
    expiry_generation: AtomicU64,
    pub(crate) guilds: Mutex<HashMap<String, Arc<GuildSession>>>,
    pub(crate) event_sequence: AtomicU64,
    pub(crate) event_tx: broadcast::Sender<serde_json::Value>,
    initial_event_rx: Mutex<Option<broadcast::Receiver<serde_json::Value>>>,
    pub(crate) closed: AtomicBool,
}

pub(crate) struct GuildSession {
    pub(crate) id: String,
    pub(crate) guild_id: String,
    pub(crate) bot_user_id: u64,
    pub(crate) playback: Arc<PlaybackSession>,
    pub(crate) voice: Mutex<Option<DiscordVoiceConnection>>,
    pub(crate) play_cancellation: Mutex<Option<CancellationToken>>,
    pub(crate) playback_operations: Arc<tokio::sync::Mutex<()>>,
    pub(crate) voice_operations: Arc<tokio::sync::Mutex<()>>,
    pub(crate) source_generation: AtomicU64,
    pub(crate) terminal_generation: AtomicU64,
    pub(crate) closed: AtomicBool,
}

pub(crate) struct ConnectionContext {
    pub(crate) id: String,
    pub(crate) principal: String,
    pub(crate) bot_user_id: u64,
    pub(crate) active_session: watch::Sender<Option<Arc<ClientSession>>>,
    pub(crate) session_control: tokio::sync::Mutex<()>,
    request_window: Mutex<(Instant, usize)>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum StateError {
    #[error("session is not available")]
    UnknownSession,
    #[error("operation is invalid in the current session state")]
    InvalidState,
    #[error("configured server capacity is exhausted")]
    ResourceExhausted,
    #[error("one or more session resources did not stop cleanly")]
    Cleanup,
}

pub(crate) fn new_inner(
    config: ServerConfig,
    observability: Observability,
    shutdown: TokioCancellationToken,
) -> Arc<ServerInner> {
    let observability_handle = observability.handle();
    Arc::new(ServerInner {
        connection_slots: Arc::new(Semaphore::new(config.limits.max_connections)),
        config: Arc::new(config),
        observability: observability_handle,
        sessions: Mutex::new(HashMap::new()),
        active_players: AtomicUsize::new(0),
        active_connections: AtomicUsize::new(0),
        node_id: opaque_id(16),
        started_at: Instant::now(),
        observability_owner: Mutex::new(Some(observability)),
        shutdown,
    })
}

impl ServerInner {
    pub(crate) fn active_player_count(&self) -> usize {
        self.active_players.load(Ordering::Acquire)
    }

    pub(crate) fn create_client(
        self: &Arc<Self>,
        principal: &str,
        bot_user_id: u64,
        connection_id: &str,
        event_buffer: usize,
    ) -> Result<(Arc<ClientSession>, String), StateError> {
        if self.shutdown.is_cancelled() {
            return Err(StateError::InvalidState);
        }
        let mut sessions = lock(&self.sessions);
        if sessions
            .values()
            .filter(|session| {
                session.principal == principal && !session.closed.load(Ordering::Acquire)
            })
            .count()
            >= self.config.limits.max_client_sessions
        {
            return Err(StateError::ResourceExhausted);
        }
        let (event_tx, event_rx) = broadcast::channel(event_buffer);
        let credential = opaque_id(32);
        let session = Arc::new(ClientSession {
            id: opaque_id(16),
            principal: principal.to_owned(),
            bot_user_id,
            resume_credential: Mutex::new(credential.clone()),
            attached_connection: Mutex::new(Some(connection_id.to_owned())),
            detached_deadline: Mutex::new(None),
            expiry_generation: AtomicU64::new(1),
            guilds: Mutex::new(HashMap::new()),
            event_sequence: AtomicU64::new(0),
            event_tx,
            initial_event_rx: Mutex::new(Some(event_rx)),
            closed: AtomicBool::new(false),
        });
        sessions.insert(session.id.clone(), Arc::clone(&session));
        Ok((session, credential))
    }

    pub(crate) fn list_clients(&self, principal: &str, bot_user_id: u64) -> Vec<serde_json::Value> {
        lock(&self.sessions)
            .values()
            .filter(|session| {
                session.principal == principal
                    && session.bot_user_id == bot_user_id
                    && !session.closed.load(Ordering::Acquire)
            })
            .map(|session| {
                serde_json::json!({
                    "sessionId": session.id,
                    "attached": lock(&session.attached_connection).is_some(),
                    "guilds": session.guild_snapshots(),
                })
            })
            .collect()
    }

    pub(crate) fn resume_client(
        &self,
        session_id: &str,
        credential: &str,
        principal: &str,
        bot_user_id: u64,
        connection_id: &str,
    ) -> Result<(Arc<ClientSession>, String), StateError> {
        let session = lock(&self.sessions)
            .get(session_id)
            .cloned()
            .ok_or(StateError::UnknownSession)?;
        if session.principal != principal
            || session.bot_user_id != bot_user_id
            || session.closed.load(Ordering::Acquire)
        {
            return Err(StateError::UnknownSession);
        }
        {
            let expected = lock(&session.resume_credential);
            let provided = credential.as_bytes();
            if expected.len() != provided.len() || !bool::from(expected.as_bytes().ct_eq(provided))
            {
                return Err(StateError::UnknownSession);
            }
        }
        let mut attached = lock(&session.attached_connection);
        if attached.is_some() {
            return Err(StateError::InvalidState);
        }
        let mut deadline = lock(&session.detached_deadline);
        if deadline.is_none_or(|deadline| deadline <= Instant::now()) {
            return Err(StateError::UnknownSession);
        }
        *attached = Some(connection_id.to_owned());
        *deadline = None;
        session.expiry_generation.fetch_add(1, Ordering::AcqRel);
        drop(deadline);
        drop(attached);
        // Keep the opaque credential stable for the lifetime of this session. If the resume
        // response is lost after attaching the new socket, rotating here would strand the client
        // with a token the server no longer accepts.
        let credential = lock(&session.resume_credential).clone();
        Ok((session, credential))
    }

    pub(crate) fn detach_client(
        self: &Arc<Self>,
        session: Arc<ClientSession>,
        connection_id: &str,
    ) {
        if session.closed.load(Ordering::Acquire) {
            return;
        }
        let mut attached = lock(&session.attached_connection);
        if attached.as_deref() != Some(connection_id) {
            return;
        }
        *attached = None;
        let deadline = Instant::now() + Duration::from_secs(self.config.resume_timeout_seconds);
        *lock(&session.detached_deadline) = Some(deadline);
        let generation = session.expiry_generation.fetch_add(1, Ordering::AcqRel) + 1;
        drop(attached);

        let server = Arc::clone(self);
        let dispatch = server.observability.dispatch().cloned();
        let expiry = async move {
            tokio::select! {
                _ = server.shutdown.cancelled() => {},
                _ = tokio::time::sleep_until(deadline.into()) => {
                    let current = session.expiry_generation.load(Ordering::Acquire) == generation;
                    let detached = lock(&session.attached_connection).is_none();
                    let expired = lock(&session.detached_deadline)
                        .is_some_and(|expires| expires <= Instant::now());
                    if current && detached && expired {
                        let _ = server.close_client(&session.id).await;
                    }
                }
            }
        };
        if let Some(dispatch) = dispatch {
            tokio::spawn(expiry.with_subscriber(dispatch));
        } else {
            tokio::spawn(expiry.with_subscriber(tracing::Dispatch::none()));
        }
    }

    pub(crate) async fn close_client(&self, session_id: &str) -> Result<(), StateError> {
        let Some(session) = lock(&self.sessions).remove(session_id) else {
            return Err(StateError::UnknownSession);
        };
        if session.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        session.expiry_generation.fetch_add(1, Ordering::AcqRel);
        *lock(&session.attached_connection) = None;
        *lock(&session.detached_deadline) = None;
        let guilds = std::mem::take(&mut *lock(&session.guilds));
        let mut cleanup_failed = false;
        for guild in guilds.into_values() {
            cleanup_failed |= self.close_guild(guild).await.is_err();
        }
        if cleanup_failed {
            Err(StateError::Cleanup)
        } else {
            Ok(())
        }
    }

    pub(crate) fn create_guild(
        &self,
        session: &Arc<ClientSession>,
        guild_id: String,
    ) -> Result<Arc<GuildSession>, StateError> {
        session.ensure_open()?;
        let mut guilds = lock(&session.guilds);
        session.ensure_open()?;
        if guilds.contains_key(&guild_id) {
            return Err(StateError::InvalidState);
        }
        if guilds.len() >= self.config.limits.max_players_per_client {
            return Err(StateError::ResourceExhausted);
        }
        let max = self.config.limits.max_total_players;
        let mut active = self.active_players.load(Ordering::Acquire);
        let reserved = loop {
            if active >= max {
                break false;
            }
            match self.active_players.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break true,
                Err(actual) => active = actual,
            }
        };
        if !reserved {
            return Err(StateError::ResourceExhausted);
        }
        let guild = Arc::new(GuildSession {
            id: opaque_id(16),
            guild_id: guild_id.clone(),
            bot_user_id: session.bot_user_id,
            playback: Arc::new(PlaybackSession::with_observability(
                self.observability.clone(),
            )),
            voice: Mutex::new(None),
            play_cancellation: Mutex::new(None),
            playback_operations: Arc::new(tokio::sync::Mutex::new(())),
            voice_operations: Arc::new(tokio::sync::Mutex::new(())),
            source_generation: AtomicU64::new(0),
            terminal_generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        guilds.insert(guild_id, Arc::clone(&guild));
        Ok(guild)
    }

    pub(crate) fn get_guild(
        &self,
        session: &Arc<ClientSession>,
        guild_id: &str,
    ) -> Result<Arc<GuildSession>, StateError> {
        session.ensure_open()?;
        lock(&session.guilds)
            .get(guild_id)
            .cloned()
            .ok_or(StateError::UnknownSession)
    }

    pub(crate) async fn destroy_guild(
        &self,
        session: &Arc<ClientSession>,
        guild_id: &str,
    ) -> Result<(), StateError> {
        let guild = lock(&session.guilds)
            .remove(guild_id)
            .ok_or(StateError::UnknownSession)?;
        self.close_guild(guild).await
    }

    pub(crate) async fn close_guild(&self, guild: Arc<GuildSession>) -> Result<(), StateError> {
        if guild.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(cancellation) = lock(&guild.play_cancellation).as_ref() {
            cancellation.cancel();
        }
        let _voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
        let playback_operation = Arc::clone(&guild.playback_operations).lock_owned().await;
        let playback = Arc::clone(&guild.playback);
        let playback_result = tokio::task::spawn_blocking(move || {
            let _playback_operation = playback_operation;
            playback.stop()
        })
        .await;
        let connection = { lock(&guild.voice).take() };
        let voice_result = if let Some(connection) = connection {
            connection.disconnect().await
        } else {
            Ok(())
        };
        self.active_players.fetch_sub(1, Ordering::AcqRel);
        if !matches!(playback_result, Ok(Ok(()))) || voice_result.is_err() {
            tracing::warn!(
                name: "server.session.cleanup_failed",
                target: "remedia_server::session",
                "one or more guild resources did not shut down cleanly"
            );
            Err(StateError::Cleanup)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.shutdown.cancel();
        let session_ids = lock(&self.sessions).keys().cloned().collect::<Vec<_>>();
        for id in session_ids {
            let _ = self.close_client(&id).await;
        }
    }

    pub(crate) fn health(&self) -> &'static str {
        if self.shutdown.is_cancelled() {
            "shutting_down"
        } else {
            "ok"
        }
    }
}

impl ConnectionContext {
    pub(crate) fn new(id: String, principal: String, bot_user_id: u64) -> Self {
        let (active_session, _) = watch::channel(None);
        Self {
            id,
            principal,
            bot_user_id,
            active_session,
            session_control: tokio::sync::Mutex::new(()),
            request_window: Mutex::new((Instant::now(), 0)),
        }
    }

    pub(crate) fn allows_request(&self, maximum_per_second: usize) -> bool {
        let mut window = lock(&self.request_window);
        if window.0.elapsed() >= Duration::from_secs(1) {
            *window = (Instant::now(), 0);
        }
        if window.1 >= maximum_per_second {
            return false;
        }
        window.1 += 1;
        true
    }

    pub(crate) fn session(&self) -> Option<Arc<ClientSession>> {
        self.active_session.borrow().clone()
    }

    pub(crate) fn set_session(&self, session: Option<Arc<ClientSession>>) {
        self.active_session.send_replace(session);
    }
}

impl ClientSession {
    pub(crate) fn ensure_open(&self) -> Result<(), StateError> {
        if self.closed.load(Ordering::Acquire) {
            Err(StateError::UnknownSession)
        } else {
            Ok(())
        }
    }

    pub(crate) fn attached_to(&self, connection_id: &str) -> bool {
        lock(&self.attached_connection).as_deref() == Some(connection_id)
            && !self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<serde_json::Value> {
        lock(&self.initial_event_rx)
            .take()
            .unwrap_or_else(|| self.event_tx.subscribe())
    }

    pub(crate) fn emit(&self, name: &str, data: serde_json::Value) {
        let sequence = self.event_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.event_tx.send(event(name, &self.id, sequence, data));
    }

    pub(crate) fn guild_snapshots(&self) -> Vec<serde_json::Value> {
        lock(&self.guilds)
            .values()
            .map(|guild| guild.snapshot())
            .collect()
    }

    pub(crate) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.id,
            "botUserId": self.bot_user_id.to_string(),
            "guilds": self.guild_snapshots(),
        })
    }
}

impl GuildSession {
    pub(crate) fn ensure_open(&self) -> Result<(), StateError> {
        if self.closed.load(Ordering::Acquire) {
            Err(StateError::UnknownSession)
        } else {
            Ok(())
        }
    }

    /// Claim the single terminal lifecycle notification for an RPC source generation.
    pub(crate) fn claim_terminal_event(&self, generation: u64) -> bool {
        let mut previous = self.terminal_generation.load(Ordering::Acquire);
        loop {
            if previous >= generation {
                return false;
            }
            match self.terminal_generation.compare_exchange_weak(
                previous,
                generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => previous = current,
            }
        }
    }

    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let voice = lock(&self.voice);
        let voice_state = voice.as_ref().map(|voice| match voice.state() {
            remedia_discord::ConnectionState::NegotiatingDave => "negotiating_dave",
            remedia_discord::ConnectionState::Connected => "connected",
            remedia_discord::ConnectionState::Reconnecting => "reconnecting",
            remedia_discord::ConnectionState::Disconnecting => "disconnecting",
            remedia_discord::ConnectionState::Disconnected => "disconnected",
            remedia_discord::ConnectionState::Failed => "failed",
        });
        serde_json::json!({
            "guildSessionId": self.id,
            "guildId": self.guild_id,
            "botUserId": self.bot_user_id.to_string(),
            "playback": playback_snapshot(
                &self.playback,
                self.source_generation.load(Ordering::Acquire),
            ),
            "voiceState": voice_state,
        })
    }
}

pub(crate) fn playback_snapshot(playback: &PlaybackSession, generation: u64) -> serde_json::Value {
    let position = playback.position();
    let media = position.media.map(|media| {
        serde_json::json!({
            "positionMs": media.position.as_millis(),
            "accuracy": match media.accuracy {
                remedia_core::MediaPositionAccuracy::Timestamp => "timestamp",
                remedia_core::MediaPositionAccuracy::Estimated => "estimated",
            },
        })
    });
    let processing_mode = playback.processing_mode().map(|mode| match mode {
        remedia_core::PlaybackProcessingMode::Passthrough => "passthrough",
        remedia_core::PlaybackProcessingMode::Transcoding => "transcoding",
    });
    let duration_ms = playback.duration().map(|duration| duration.as_millis());
    let last_error = playback.last_error().map(|_| {
        serde_json::json!({
            "code": "playback_failed",
            "message": "Playback failed; see server diagnostics for details.",
        })
    });
    let source = playback.info().map(|info| {
        serde_json::json!({
            "codec": info.codec,
            "sampleRate": info.sample_rate,
            "channels": info.channels,
            "channelLayout": info.channel_layout,
            "durationMs": info.duration.map(|duration| duration.as_millis()),
            "bitRate": info.bit_rate,
            "streamIndex": info.stream_index,
            "metadata": info.metadata,
        })
    });
    serde_json::json!({
        "state": playback_state_name(playback.state()),
        "generation": generation,
        "processingMode": processing_mode,
        "position": {
            "media": media,
            "submittedMs": position.submitted.as_millis(),
            "playedMs": position.played.map(|duration| duration.as_millis()),
        },
        "durationMs": duration_ms,
        "source": source,
        "filterGraph": playback.filter_graph(),
        "volume": playback.volume(),
        "lastError": last_error,
    })
}

pub(crate) fn playback_state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Idle => "idle",
        PlaybackState::Starting => "starting",
        PlaybackState::Playing => "playing",
        PlaybackState::Pausing => "pausing",
        PlaybackState::Resuming => "resuming",
        PlaybackState::Paused => "paused",
        PlaybackState::Stopping => "stopping",
        PlaybackState::Stopped => "stopped",
        PlaybackState::Completed => "completed",
        PlaybackState::Failed => "failed",
    }
}

pub(crate) fn opaque_id(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    random::<[u8; 32]>()
        .iter()
        .cycle()
        .take(bytes)
        .zip(value.iter_mut())
        .for_each(|(source, target)| *target = *source);
    let mut result = String::with_capacity(bytes * 2);
    for byte in value {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::opaque_id;

    #[test]
    fn opaque_ids_have_expected_entropy_length_and_hex_format() {
        let id = opaque_id(32);
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(id, opaque_id(32));
    }
}
