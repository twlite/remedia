use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use remedia_core::{CancellationToken, EncodedOpusPacket, OpusPacketSink, OutputError};

use crate::{ConnectionState, DiscordOpusSender};

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
    generation: u64,
    target: Option<Arc<DiscordOpusSender>>,
    failure: Option<String>,
}

/// Playback-scoped packet sink. Cancelling it never closes the shared voice
/// connection. It waits without polling while a replacement transport starts.
pub struct RoutedOpusSender {
    router: DiscordOpusSenderRouter,
    cancelled: AtomicBool,
}

impl DiscordOpusSenderRouter {
    /// Create a router without an attached transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a separate sink handle for one playback output.
    pub fn sink(&self) -> RoutedOpusSender {
        RoutedOpusSender {
            router: self.clone(),
            cancelled: AtomicBool::new(false),
        }
    }

    /// Attach a ready transport and resume parked playback.
    pub fn attach(&self, sender: DiscordOpusSender) {
        let mut state = lock(&self.inner.state);
        state.generation = state.generation.wrapping_add(1);
        state.target = Some(Arc::new(sender));
        state.failure = None;
        tracing::debug!(
            event = "voice_output_router_attached",
            transport_generation = state.generation,
            "attached the active Discord packet sender"
        );
        self.inner.changed.notify_all();
    }

    /// Stop routing to the old transport during migration. Accepted packets in
    /// its bounded queue may be lost when it is closed; none are replayed with
    /// stale encryption keys.
    pub fn detach(&self) {
        let mut state = lock(&self.inner.state);
        state.generation = state.generation.wrapping_add(1);
        state.target = None;
        // A new replacement attempt returns the router to its retryable parked
        // state, including after an earlier recovery attempt was terminally
        // reported and the application explicitly connects again.
        state.failure = None;
        tracing::debug!(
            event = "voice_output_router_detached",
            transport_generation = state.generation,
            "parked playback output for Discord transport replacement"
        );
        self.inner.changed.notify_all();
    }

    /// Fail parked playback after the connection manager exhausts recovery or
    /// an explicit shutdown makes recovery impossible.
    pub fn fail(&self, message: impl Into<String>) {
        let mut state = lock(&self.inner.state);
        state.generation = state.generation.wrapping_add(1);
        state.target = None;
        state.failure = Some(message.into());
        tracing::warn!(
            event = "voice_output_router_failed",
            transport_generation = state.generation,
            "could not restore Discord packet delivery"
        );
        self.inner.changed.notify_all();
    }

    fn target(
        &self,
        cancelled: &AtomicBool,
        cancellation: &CancellationToken,
    ) -> Result<(u64, Arc<DiscordOpusSender>), OutputError> {
        let mut state = lock(&self.inner.state);
        loop {
            if cancelled.load(Ordering::Acquire) || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            if let Some(message) = &state.failure {
                return Err(OutputError::new(message.clone()));
            }
            if let Some(target) = state.target.as_ref() {
                return Ok((state.generation, Arc::clone(target)));
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
            let (generation, target) = self.router.target(&self.cancelled, cancellation)?;
            match action(&target) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let mut state = lock(&self.router.inner.state);
                    let replaced = state.generation != generation;
                    let failed_transport = matches!(
                        target.state(),
                        ConnectionState::Disconnected | ConnectionState::Failed
                    );
                    if state.generation == generation && failed_transport {
                        state.generation = state.generation.wrapping_add(1);
                        state.target = None;
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
        self.with_target(cancellation, |target| target.finish(cancellation))
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _state = lock(&self.router.inner.state);
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
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn cancelling_a_parked_output_releases_its_packet_waiter() {
        let router = DiscordOpusSenderRouter::new();
        let sink = Arc::new(router.sink());
        let waiting_sink = Arc::clone(&sink);
        let cancellation = CancellationToken::new();
        let (finished, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            let packet = EncodedOpusPacket {
                bytes: vec![0xF8, 0xFF, 0xFE],
                samples_per_channel: 960,
                media_samples_per_channel: 960,
                start_sample: 0,
            };
            let outcome = waiting_sink.send_packet(packet, &cancellation);
            let _ = finished.send(outcome);
        });

        sink.cancel();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(1)),
            Ok(Err(OutputError::Cancelled))
        ));
        worker.join().expect("parked sender worker should exit");
    }

    #[test]
    fn passthrough_capability_survives_a_detached_transport() {
        let router = DiscordOpusSenderRouter::new();
        let sink = router.sink();
        let packet = EncodedOpusPacket {
            bytes: vec![0xF8, 0xFF, 0xFE],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        };

        assert!(sink.supports_passthrough_packet(&packet));
        router.detach();
        assert!(sink.supports_passthrough_packet(&packet));
    }

    #[test]
    fn detaching_for_a_new_attempt_clears_a_previous_terminal_failure() {
        let router = DiscordOpusSenderRouter::new();
        router.fail("previous recovery failed");

        router.detach();

        let state = lock(&router.inner.state);
        assert!(state.target.is_none());
        assert!(state.failure.is_none());
    }
}
