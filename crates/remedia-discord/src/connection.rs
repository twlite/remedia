use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use flume::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use rand::random;
use remedia_core::{
    CancellationToken as CoreCancellationToken, EncodedOpusPacket, OpusPacketSink, OutputError,
};
use remedia_observability::{ObservabilityHandle, next_correlation_id};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken as TokioCancellationToken;
use tracing::Instrument;
use tracing::instrument::WithSubscriber;
use url::Url;

use crate::crypto::EncryptionMode;
use crate::dave::{DaveProtocol, OutgoingGatewayMessage};
use crate::error::VoiceError;
use crate::protocol::{
    GatewayMessage, ip_discovery_request, json_message, parse_ip_discovery_response, parse_u16,
};
use crate::rtp::{MAX_UDP_PACKET_BYTES, RTP_HEADER_BYTES, RtpSender, validate_opus_packet};
use crate::with_dispatch;

const GATEWAY_VERSION: u16 = 8;
const DAVE_PROTOCOL_VERSION: u16 = davey::DAVE_PROTOCOL_VERSION;
const QUEUE_CAPACITY: usize = 3;
const PACKET_RATE: u32 = 48_000;
const SILENCE_PACKET: [u8; 3] = [0xF8, 0xFF, 0xFE];
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_millis(25);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(12);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const UDP_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

type VoiceWebSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Information delivered by the caller's Discord Gateway voice-state flow.
///
/// `token` and `session_id` are secrets. The custom debug output redacts them.
#[derive(Clone)]
pub struct VoiceConnectionInfo {
    /// Discord guild or server snowflake.
    pub guild_id: u64,
    /// Voice channel snowflake. DAVE MLS binds its group to this channel.
    pub channel_id: u64,
    /// Bot user snowflake.
    pub user_id: u64,
    /// Voice session ID received from the Gateway.
    pub session_id: String,
    /// Voice server token received from the Gateway.
    pub token: String,
    /// Voice endpoint supplied by the Gateway, with or without `wss://`.
    pub endpoint: String,
}

impl fmt::Debug for VoiceConnectionInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoiceConnectionInfo")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("session_id", &"[redacted]")
            .field("token", &"[redacted]")
            .field("endpoint", &"[redacted]")
            .finish()
    }
}

/// Observable lifecycle state for one established voice transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    /// The voice transport is established and DAVE MLS setup is in progress.
    NegotiatingDave,
    /// The voice gateway and UDP transport are established.
    Connected,
    /// The voice gateway is attempting a protocol Resume.
    Reconnecting,
    /// The voice worker is shutting down.
    Disconnecting,
    /// The transport has been explicitly closed.
    Disconnected,
    /// The transport failed and cannot continue safely.
    Failed,
}

/// An established outbound Discord voice connection.
///
/// Connections own their sockets and DAVE state independently of playback.
/// Dropping the last handle cancels the transport worker; explicit
/// [`disconnect`](Self::disconnect) waits for it to release its resources.
#[derive(Clone)]
pub struct DiscordVoiceConnection {
    inner: Arc<ConnectionInner>,
}

struct ConnectionInner {
    voice_connection_id: u64,
    guild_id: u64,
    observability: ObservabilityHandle,
    state: watch::Sender<ConnectionState>,
    failure: Arc<Mutex<Option<Arc<VoiceError>>>>,
    packet_tx: Sender<QueuedPacket>,
    control_tx: Sender<Control>,
    generation: AtomicU64,
    generation_update: Mutex<()>,
    generation_tx: watch::Sender<u64>,
    cancelled_generation: Arc<AtomicU64>,
    accepted_packet_id: AtomicU64,
    next_packet_id: AtomicU64,
    cancellation: TokioCancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        with_dispatch(&self.observability, || {
            tracing::debug!(
                name: "discord.connection.handle_dropped",
                target: "remedia_discord::connection",
                voice_connection_id = self.voice_connection_id,
                "last voice connection handle dropped; transport cancellation requested"
            );
        });
        self.cancellation.cancel();
    }
}

struct QueuedPacket {
    generation: u64,
    id: u64,
    packet: EncodedOpusPacket,
}

enum Control {
    Flush {
        generation: u64,
        ack: mpsc::SyncSender<Result<(), VoiceError>>,
    },
    Pause {
        target_id: u64,
        ack: mpsc::SyncSender<Result<(), VoiceError>>,
    },
    Resume {
        ack: mpsc::SyncSender<Result<(), VoiceError>>,
    },
    Finish {
        target_id: u64,
        ack: mpsc::SyncSender<Result<(), VoiceError>>,
    },
    Cancel {
        generation: u64,
    },
}

enum BarrierKind {
    Pause,
    Finish,
    Cancel,
}

struct PendingBarrier {
    target_id: u64,
    silence_packets: u8,
    kind: BarrierKind,
    ack: Option<mpsc::SyncSender<Result<(), VoiceError>>>,
}

struct RtpSendObservability {
    voice_connection_id: u64,
    metrics: remedia_observability::RemediaMetrics,
}

struct ActiveVoiceConnection {
    metrics: remedia_observability::RemediaMetrics,
    active: bool,
}

impl ActiveVoiceConnection {
    fn new(metrics: remedia_observability::RemediaMetrics) -> Self {
        Self {
            metrics,
            active: false,
        }
    }

    fn mark_connected(&mut self) {
        if !self.active {
            self.active = true;
            self.metrics.voice_connection_active(true);
        }
    }
}

impl Drop for ActiveVoiceConnection {
    fn drop(&mut self) {
        if self.active {
            self.metrics.voice_connection_active(false);
        }
    }
}

fn set_connection_state(
    state: &watch::Sender<ConnectionState>,
    next: ConnectionState,
    voice_connection_id: u64,
    guild_id: u64,
) {
    let previous = *state.borrow();
    if previous == next {
        return;
    }
    state.send_replace(next);
    tracing::info!(
        name: "discord.connection.state_changed",
        target: "remedia_discord::connection",
        voice_connection_id,
        guild_id,
        previous_state = ?previous,
        state = ?next,
        "Discord voice connection state changed"
    );
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) struct TestVoiceConnection {
    connection: DiscordVoiceConnection,
    packets: Arc<Mutex<Vec<(u64, EncodedOpusPacket)>>>,
    packets_changed: Arc<Condvar>,
    controls: Arc<Mutex<Vec<&'static str>>>,
    actor: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl TestVoiceConnection {
    pub(crate) fn new(guild_id: u64) -> Self {
        let (packet_tx, packet_rx) = flume::bounded(QUEUE_CAPACITY);
        let (control_tx, control_rx) = flume::bounded(16);
        let (generation_tx, _generation_rx) = watch::channel(0);
        let (state, _) = watch::channel(ConnectionState::Connected);
        let cancellation = TokioCancellationToken::new();
        let inner = Arc::new(ConnectionInner {
            voice_connection_id: next_correlation_id(),
            guild_id,
            observability: ObservabilityHandle::default(),
            state,
            failure: Arc::new(Mutex::new(None)),
            packet_tx,
            control_tx,
            generation: AtomicU64::new(0),
            generation_update: Mutex::new(()),
            generation_tx,
            cancelled_generation: Arc::new(AtomicU64::new(u64::MAX)),
            accepted_packet_id: AtomicU64::new(0),
            next_packet_id: AtomicU64::new(0),
            cancellation: cancellation.clone(),
            worker: Mutex::new(None),
        });
        let packets = Arc::new(Mutex::new(Vec::new()));
        let packets_changed = Arc::new(Condvar::new());
        let controls = Arc::new(Mutex::new(Vec::new()));
        let actor_packets = Arc::clone(&packets);
        let actor_packets_changed = Arc::clone(&packets_changed);
        let actor_controls = Arc::clone(&controls);
        let actor_cancellation = cancellation.clone();
        let actor = std::thread::spawn(move || {
            let mut processed_id = 0;
            let mut pending_barriers = Vec::new();
            while !actor_cancellation.is_cancelled() {
                match packet_rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(packet) => {
                        processed_id = processed_id.max(packet.id);
                        lock(&actor_packets).push((packet.generation, packet.packet));
                        actor_packets_changed.notify_all();
                    }
                    Err(flume::RecvTimeoutError::Timeout) => {}
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                }
                while let Ok(control) = control_rx.try_recv() {
                    match control {
                        Control::Flush { ack, .. } => {
                            lock(&actor_controls).push("flush");
                            let _ = ack.send(Ok(()));
                        }
                        Control::Pause { target_id, ack } => {
                            lock(&actor_controls).push("pause");
                            pending_barriers.push((target_id, ack));
                        }
                        Control::Resume { ack } => {
                            lock(&actor_controls).push("resume");
                            let _ = ack.send(Ok(()));
                        }
                        Control::Finish { target_id, ack } => {
                            lock(&actor_controls).push("finish");
                            pending_barriers.push((target_id, ack));
                        }
                        Control::Cancel { .. } => lock(&actor_controls).push("cancel"),
                    }
                }
                // Controls and packets use separate channels. Defer acknowledgements
                // until the target packet is recorded, but keep receiving both channels
                // so a control observed before its packets cannot deadlock the actor.
                pending_barriers.retain(|(target_id, ack)| {
                    if processed_id >= *target_id {
                        let _ = ack.send(Ok(()));
                        false
                    } else {
                        true
                    }
                });
            }
        });
        Self {
            connection: DiscordVoiceConnection { inner },
            packets,
            packets_changed,
            controls,
            actor: Some(actor),
        }
    }

    pub(crate) fn connection(&self) -> DiscordVoiceConnection {
        self.connection.clone()
    }

    pub(crate) fn packets(&self) -> Vec<(u64, EncodedOpusPacket)> {
        lock(&self.packets).clone()
    }

    /// Wait for the mock receiver to record queued packets, not merely for
    /// send_packet() to return after enqueueing them.
    pub(crate) fn wait_for_packets(&self, count: usize) -> Vec<(u64, EncodedOpusPacket)> {
        let packets = lock(&self.packets);
        let (packets, _) = self
            .packets_changed
            .wait_timeout_while(packets, Duration::from_secs(2), |packets| {
                packets.len() < count
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            packets.len() >= count,
            "mock voice worker received {} packets; expected at least {count}",
            packets.len()
        );
        packets.clone()
    }

    pub(crate) fn controls(&self) -> Vec<&'static str> {
        lock(&self.controls).clone()
    }
}

#[cfg(test)]
impl Drop for TestVoiceConnection {
    fn drop(&mut self) {
        self.connection.inner.cancellation.cancel();
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
    }
}

#[cfg(test)]
mod tests;

mod session;

mod sender;
pub use sender::DiscordOpusSender;

mod handshake;
use handshake::*;

mod actor;
use actor::*;

mod resume;
use resume::*;

mod transport;
use transport::*;
