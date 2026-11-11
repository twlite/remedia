use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rand::random;
use remedia_core::{CancellationToken, PlaybackSession, PlaybackState};
use remedia_discord::{DiscordOpusSenderRouter, DiscordVoiceConnection};
use remedia_observability::{Observability, ObservabilityHandle};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::sync::watch;
use tokio::sync::{Semaphore, broadcast};
use tokio_util::sync::CancellationToken as TokioCancellationToken;
use tracing::instrument::WithSubscriber;

use crate::config::ServerConfig;
use crate::plugin_host::PluginHostRegistry;
use crate::protocol::event;

pub(crate) struct ServerInner {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) plugin_hosts: Arc<PluginHostRegistry>,
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
    /// Cancelled when the session closes, so its pending forwarded plugin operations end with it.
    pub(crate) scope: TokioCancellationToken,
    initial_event_rx: Mutex<Option<broadcast::Receiver<serde_json::Value>>>,
    pub(crate) closed: AtomicBool,
}

pub(crate) struct GuildSession {
    pub(crate) id: String,
    pub(crate) guild_id: String,
    pub(crate) bot_user_id: u64,
    pub(crate) playback: Arc<PlaybackSession>,
    pub(crate) voice: Mutex<Option<DiscordVoiceConnection>>,
    pub(crate) voice_sender_router: DiscordOpusSenderRouter,
    pub(crate) voice_generation: AtomicU64,
    pub(crate) voice_request_generation: AtomicU64,
    pub(crate) voice_connected_generation: AtomicU64,
    pub(crate) voice_terminal_generation: AtomicU64,
    pub(crate) voice_reconnecting: AtomicBool,
    pub(crate) play_cancellation: Mutex<Option<PlayCancellation>>,
    /// The operation currently resolving a staged source, if any.
    pub(crate) stage_scope: Mutex<Option<StageScope>>,
    pub(crate) playback_operations: Arc<tokio::sync::Mutex<()>>,
    pub(crate) voice_operations: Arc<tokio::sync::Mutex<()>>,
    pub(crate) source_generation: AtomicU64,
    pub(crate) terminal_generation: AtomicU64,
    pub(crate) closed: AtomicBool,
}

/// Cancels both the native playback start and any plugin operation which precedes it.
///
/// A source is first resolved (possibly through a plugin host) and then opened by the
/// decoder. Stop, skip, replacement, guild destruction and shutdown must be able to
/// interrupt either phase through one handle.
#[derive(Clone, Default)]
pub(crate) struct PlayCancellation {
    pub(crate) core: CancellationToken,
    pub(crate) scope: TokioCancellationToken,
}

impl PlayCancellation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.scope.cancel();
        self.core.cancel();
    }
}

/// Identity of a staged-source preparation which has not reached the decoder yet.
#[derive(Clone)]
pub(crate) struct StageScope {
    pub(crate) id: u64,
    pub(crate) token: TokioCancellationToken,
    pub(crate) expected_generation: u64,
    pub(crate) entry_id: String,
    pub(crate) queue_revision: u64,
    pub(crate) source_revision: u64,
}

pub(crate) struct ConnectionContext {
    pub(crate) id: String,
    pub(crate) principal: String,
    pub(crate) bot_user_id: u64,
    pub(crate) active_session: watch::Sender<Option<Arc<ClientSession>>>,
    pub(crate) session_control: tokio::sync::Mutex<()>,
    /// Cancelled when the WebSocket closes; pending plugin operations observe it.
    pub(crate) closed: TokioCancellationToken,
    pub(crate) probes: Mutex<std::collections::HashMap<String, remedia_core::CancellationToken>>,
    /// Pending `plugin.list` and `plugin.execute` requests of this connection, by request ID.
    /// Each token is a child of `closed`. Entries exist only while a request is in flight, so
    /// the map is bounded by `maxInFlightRequests`, and only this connection can cancel them.
    pub(crate) plugin_requests: Mutex<std::collections::HashMap<String, TokioCancellationToken>>,
    request_window: Mutex<(Instant, usize)>,
}

impl GuildSession {
    /// Cancel an unfinished staged-source preparation, including any plugin operation.
    pub(crate) fn cancel_pending_stage(&self) {
        if let Some(scope) = lock(&self.stage_scope).take() {
            scope.token.cancel();
        }
    }
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
    plugin_hosts: PluginHostRegistry,
    observability: Observability,
    shutdown: TokioCancellationToken,
) -> Arc<ServerInner> {
    let observability_handle = observability.handle();
    Arc::new(ServerInner {
        plugin_hosts: Arc::new(plugin_hosts),
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
            scope: TokioCancellationToken::new(),
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
        session.scope.cancel();
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
            voice_sender_router: DiscordOpusSenderRouter::new(),
            voice_generation: AtomicU64::new(0),
            voice_request_generation: AtomicU64::new(0),
            voice_connected_generation: AtomicU64::new(0),
            voice_terminal_generation: AtomicU64::new(0),
            voice_reconnecting: AtomicBool::new(false),
            play_cancellation: Mutex::new(None),
            stage_scope: Mutex::new(None),
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
        guild.cancel_pending_stage();
        let _voice_operation = Arc::clone(&guild.voice_operations).lock_owned().await;
        let playback_operation = Arc::clone(&guild.playback_operations).lock_owned().await;
        let playback = Arc::clone(&guild.playback);
        let playback_result = tokio::task::spawn_blocking(move || {
            let _playback_operation = playback_operation;
            playback.stop()
        })
        .await;
        let connection = { lock(&guild.voice).take() };
        guild.voice_generation.fetch_add(1, Ordering::AcqRel);
        guild.voice_reconnecting.store(false, Ordering::Release);
        guild
            .voice_sender_router
            .shutdown("guild session was closed");
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
        self.plugin_hosts.close().await;
    }

    pub(crate) fn health(&self) -> &'static str {
        if self.shutdown.is_cancelled() {
            "shutting_down"
        } else {
            "ok"
        }
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
mod tests;

mod connection;

mod sessions;

mod snapshot;
pub(crate) use snapshot::*;

impl ServerInner {
    /// Begin background discovery for configured plugin hosts.
    pub(crate) fn start_plugin_hosts(&self) {
        self.plugin_hosts.start();
    }
}
