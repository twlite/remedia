use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use remedia_core::{CancellationToken, EncodedOpusPacket, OpusPacketSink, OutputError};

use crate::{ConnectionState, DiscordOpusSender, DiscordVoiceConnection};

/// A stable, bounded handoff from playback to the current Discord transport.
/// Replacing a transport does not replace or restart the playback session.
#[derive(Clone, Default)]
pub struct DiscordOpusSenderRouter {
    inner: Arc<RouterInner>,
}

#[derive(Default)]
struct RouterInner {
    state: Mutex<RouterState>,
    changed: Condvar,
}

#[derive(Default)]
struct RouterState {
    transport_generation: u64,
    playback_generation: u64,
    active_playback_generation: Option<u64>,
    transport_state: RouterTransportState,
    connection: Option<DiscordVoiceConnection>,
    target: Option<Arc<DiscordOpusSender>>,
    failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RouterTransportState {
    #[default]
    Disconnected,
    Connected,
    Reconnecting,
    Failed,
}

/// Playback-scoped packet sink. Cancelling it never closes the shared voice
/// connection. It waits without polling while a replacement transport starts.
pub struct RoutedOpusSender {
    router: DiscordOpusSenderRouter,
    playback_generation: u64,
    cancelled: AtomicBool,
}

impl DiscordOpusSenderRouter {
    /// Create a router without an attached transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a separate sink handle for one playback output.
    pub fn sink(&self) -> RoutedOpusSender {
        let mut state = lock(&self.inner.state);
        state.playback_generation = state.playback_generation.wrapping_add(1);
        let playback_generation = state.playback_generation;
        let target = state
            .connection
            .as_ref()
            .map(|connection| Arc::new(connection.opus_sender()));
        state.active_playback_generation = Some(playback_generation);
        state.target = target;
        let sender_generation = state.target.as_ref().map(|sender| sender.generation());
        tracing::debug!(
            event = "voice_output_playback_started",
            guild_id = state
                .connection
                .as_ref()
                .map(DiscordVoiceConnection::guild_id),
            playback_generation,
            sender_generation,
            transport_generation = state.transport_generation,
            "created a fresh sender generation for playback"
        );
        self.inner.changed.notify_all();
        RoutedOpusSender {
            router: self.clone(),
            playback_generation,
            cancelled: AtomicBool::new(false),
        }
    }

    /// Attach a ready playback-scoped sender and resume parked playback.
    ///
    /// The router retains its voice connection and creates a fresh sender for
    /// each later playback, so finishing this sender cannot poison the next one.
    pub fn attach(&self, sender: DiscordOpusSender) {
        let connection = sender.voice_connection();
        let mut state = lock(&self.inner.state);
        state.transport_generation = state.transport_generation.wrapping_add(1);
        let target = state.active_playback_generation.map(|_| Arc::new(sender));
        let sender_generation = target.as_ref().map(|sender| sender.generation());
        let guild_id = connection.guild_id();
        let previous_state = state.transport_state;
        let previous_connection_id = state
            .connection
            .as_ref()
            .map(DiscordVoiceConnection::voice_connection_id);
        state.connection = Some(connection);
        state.target = target;
        state.failure = None;
        state.transport_state = RouterTransportState::Connected;
        tracing::debug!(
            event = "voice_output_router_attached",
            guild_id,
            playback_generation = state.active_playback_generation,
            sender_generation,
            previous_state = ?previous_state,
            state = ?state.transport_state,
            previous_connection_id,
            transport_generation = state.transport_generation,
            "attached a ready Discord transport"
        );
        self.inner.changed.notify_all();
    }

    /// Stop routing to the old transport during migration. Accepted packets in
    /// its bounded queue may be lost when it is closed; none are replayed with
    /// stale encryption keys.
    pub fn detach(&self) {
        let mut state = lock(&self.inner.state);
        state.transport_generation = state.transport_generation.wrapping_add(1);
        let previous_state = state.transport_state;
        state.target = None;
        state.transport_state = RouterTransportState::Reconnecting;
        // A new replacement attempt returns the router to its retryable parked
        // state, including after an earlier recovery attempt was terminally
        // reported and the application explicitly connects again.
        state.failure = None;
        tracing::debug!(
            event = "voice_output_router_detached",
            guild_id = state
                .connection
                .as_ref()
                .map(DiscordVoiceConnection::guild_id),
            playback_generation = state.active_playback_generation,
            previous_state = ?previous_state,
            state = ?state.transport_state,
            transport_generation = state.transport_generation,
            "parked playback output for Discord transport replacement"
        );
        self.inner.changed.notify_all();
    }

    /// Fail parked playback after the connection manager exhausts recovery or
    /// an explicit shutdown makes recovery impossible.
    pub fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        let mut state = lock(&self.inner.state);
        state.transport_generation = state.transport_generation.wrapping_add(1);
        let previous_state = state.transport_state;
        state.target = None;
        state.failure = Some(message.clone());
        state.transport_state = RouterTransportState::Failed;
        tracing::warn!(
            event = "voice_output_router_failed",
            guild_id = state
                .connection
                .as_ref()
                .map(DiscordVoiceConnection::guild_id),
            playback_generation = state.active_playback_generation,
            transport_generation = state.transport_generation,
            previous_state = ?previous_state,
            state = ?state.transport_state,
            error = %message,
            "could not restore Discord packet delivery"
        );
        self.inner.changed.notify_all();
    }

    /// Stop routing because the owning voice session is intentionally closing.
    pub fn shutdown(&self, message: impl Into<String>) {
        let message = message.into();
        let mut state = lock(&self.inner.state);
        state.transport_generation = state.transport_generation.wrapping_add(1);
        let previous_state = state.transport_state;
        state.target = None;
        state.failure = Some(message.clone());
        state.transport_state = RouterTransportState::Failed;
        tracing::info!(
            event = "voice_output_router_shutdown",
            guild_id = state.connection.as_ref().map(DiscordVoiceConnection::guild_id),
            playback_generation = state.active_playback_generation,
            transport_generation = state.transport_generation,
            previous_state = ?previous_state,
            state = ?state.transport_state,
            reason = %message,
            "stopped the Discord output router during voice session cleanup"
        );
        self.inner.changed.notify_all();
    }

    fn target(
        &self,
        playback_generation: u64,
        cancelled: &AtomicBool,
        cancellation: &CancellationToken,
    ) -> Result<(u64, Arc<DiscordOpusSender>), OutputError> {
        let mut state = lock(&self.inner.state);
        loop {
            if cancelled.load(Ordering::Acquire) || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            if state.active_playback_generation != Some(playback_generation) {
                return Err(OutputError::Cancelled);
            }
            if let Some(message) = &state.failure {
                return Err(OutputError::new(message.clone()));
            }
            if let Some(target) = state.target.as_ref() {
                return Ok((state.transport_generation, Arc::clone(target)));
            }
            state = self
                .inner
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl RoutedOpusSender {
    fn with_target<T>(
        &self,
        cancellation: &CancellationToken,
        action: impl Fn(&DiscordOpusSender) -> Result<T, OutputError>,
    ) -> Result<T, OutputError> {
        loop {
            if self.cancelled.load(Ordering::Acquire) || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let (generation, target) =
                self.router
                    .target(self.playback_generation, &self.cancelled, cancellation)?;
            match action(&target) {
                Ok(value) => {
                    let state = lock(&self.router.inner.state);
                    if state.active_playback_generation != Some(self.playback_generation) {
                        return Err(OutputError::Cancelled);
                    }
                    return Ok(value);
                }
                Err(error) => {
                    let mut state = lock(&self.router.inner.state);
                    let replaced = state.transport_generation != generation;
                    let failed_transport = matches!(
                        target.state(),
                        ConnectionState::Disconnected | ConnectionState::Failed
                    );
                    if state.transport_generation == generation && failed_transport {
                        state.transport_generation = state.transport_generation.wrapping_add(1);
                        state.target = None;
                        let previous_state = state.transport_state;
                        state.transport_state = RouterTransportState::Reconnecting;
                        tracing::warn!(
                            event = "voice_output_transport_lost",
                            guild_id = state.connection.as_ref().map(DiscordVoiceConnection::guild_id),
                            playback_generation = self.playback_generation,
                            sender_generation = target.generation(),
                            transport_generation = state.transport_generation,
                            previous_state = ?previous_state,
                            state = ?state.transport_state,
                            error = %error,
                            "Discord packet delivery is parked while transport recovers"
                        );
                        self.router.inner.changed.notify_all();
                    }
                    drop(state);
                    if replaced || failed_transport {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }
}

impl OpusPacketSink for RoutedOpusSender {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.with_target(cancellation, |target| {
            target.send_packet(packet.clone(), cancellation)
        })
    }

    fn supports_passthrough_packet(&self, packet: &EncodedOpusPacket) -> bool {
        // Capability is a property of the packet contract, not of whether a
        // transport is momentarily attached. Keep Opus passthrough selected
        // while migration parks delivery at the bounded output boundary.
        DiscordOpusSender::supports_passthrough_packet(packet)
    }

    fn flush(
        &self,
        position: std::time::Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.with_target(cancellation, |target| target.flush(position, cancellation))
    }

    fn flush_samples(
        &self,
        sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.with_target(cancellation, |target| {
            target.flush_samples(sample_position, cancellation)
        })
    }

    fn pause(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.with_target(cancellation, |target| target.pause(cancellation))
    }

    fn resume(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.with_target(cancellation, |target| target.resume(cancellation))
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.with_target(cancellation, |target| target.finish(cancellation))?;
        let mut state = lock(&self.router.inner.state);
        if state.active_playback_generation == Some(self.playback_generation) {
            let sender_generation = state.target.as_ref().map(|sender| sender.generation());
            tracing::debug!(
                event = "voice_output_playback_finished",
                guild_id = state
                    .connection
                    .as_ref()
                    .map(DiscordVoiceConnection::guild_id),
                playback_generation = self.playback_generation,
                sender_generation,
                transport_generation = state.transport_generation,
                "finished playback sender while retaining the voice transport"
            );
            state.active_playback_generation = None;
            state.target = None;
            self.router.inner.changed.notify_all();
        }
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut state = lock(&self.router.inner.state);
        if state.active_playback_generation == Some(self.playback_generation) {
            let sender_generation = state.target.as_ref().map(|sender| sender.generation());
            tracing::debug!(
                event = "voice_output_playback_cancelled",
                guild_id = state
                    .connection
                    .as_ref()
                    .map(DiscordVoiceConnection::guild_id),
                playback_generation = self.playback_generation,
                sender_generation,
                transport_generation = state.transport_generation,
                "cancelled the current playback sender"
            );
            state.active_playback_generation = None;
            state.target = None;
        }
        self.router.inner.changed.notify_all();
    }

    fn played_position(&self) -> Option<std::time::Duration> {
        lock(&self.router.inner.state)
            .target
            .as_ref()
            .and_then(|target| target.played_position())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
