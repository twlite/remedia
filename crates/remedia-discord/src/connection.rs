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

impl DiscordVoiceConnection {
    /// Connect to Discord voice using session information from an external
    /// Gateway client. This performs the voice WebSocket handshake, UDP IP
    /// discovery, encryption selection, and DAVE setup.
    pub async fn connect(info: VoiceConnectionInfo) -> Result<Self, VoiceError> {
        Self::connect_with_observability(info, ObservabilityHandle::default()).await
    }

    /// Connect using an application-owned Remedia observability context.
    ///
    /// The context is attached only to this connection's work and does not install or replace a
    /// process-global tracing subscriber.
    pub async fn connect_with_observability(
        info: VoiceConnectionInfo,
        observability: ObservabilityHandle,
    ) -> Result<Self, VoiceError> {
        let voice_connection_id = next_correlation_id();
        let guild_id = info.guild_id;
        let started_at = Instant::now();
        let metrics = observability.metrics();
        metrics.voice_connection_attempt();
        let dispatch = observability.dispatch().cloned();
        let span = with_dispatch(&observability, || {
            tracing::info_span!(
                target: "remedia_discord::connection",
                "discord.connection.establish",
                voice_connection_id,
                guild_id,
                otel.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            )
        });
        let run = async move {
            tracing::info!(
                name: "discord.connection.attempt",
                target: "remedia_discord::connection",
                voice_connection_id,
                "Discord voice connection attempt started"
            );
            let result = Self::connect_inner(
                info,
                observability,
                voice_connection_id,
                started_at,
                metrics.clone(),
            )
            .await;
            if let Err(error) = &result {
                tracing::Span::current().record("otel.status_code", "ERROR");
                tracing::Span::current().record("error.type", error.kind());
                tracing::error!(
                    name: "discord.connection.failed",
                    target: "remedia_discord::connection",
                    voice_connection_id,
                    error_kind = error.kind(),
                    "Discord voice connection attempt failed"
                );
            } else {
                tracing::Span::current().record("otel.status_code", "OK");
            }
            result
        }
        .instrument(span);
        match dispatch {
            Some(dispatch) => run.with_subscriber(dispatch).await,
            None => run.await,
        }
    }

    async fn connect_inner(
        info: VoiceConnectionInfo,
        observability: ObservabilityHandle,
        connection_id: u64,
        started_at: Instant,
        metrics: remedia_observability::RemediaMetrics,
    ) -> Result<Self, VoiceError> {
        let endpoint = voice_endpoint(&info.endpoint)?;
        let guild_id = info.guild_id;
        let (mut websocket, hello_interval, ready, early_messages, session_data, udp, sequence_ack) =
            establish_voice_session(&endpoint, &info, connection_id).await?;

        let dave_version = session_data
            .get("dave_protocol_version")
            .map(|value| parse_u16(value, "dave_protocol_version"))
            .transpose()?
            .ok_or_else(|| {
                VoiceError::Protocol(
                    "voice session description omitted `dave_protocol_version`".into(),
                )
            })?;
        if dave_version != DAVE_PROTOCOL_VERSION {
            return Err(VoiceError::UnsupportedDaveVersion(dave_version));
        }
        tracing::info!(
            name: "discord.dave.protocol_negotiated",
            target: "remedia_discord::connection",
            voice_connection_id = connection_id,
            protocol_version = dave_version,
            "mandatory DAVE protocol version negotiated"
        );
        let secret_key = parse_secret_key(&session_data)?;
        let mode_name = session_data
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                VoiceError::Protocol("session description omitted encryption mode".into())
            })?;
        let mode = EncryptionMode::choose(&ready.modes)?;
        if mode.wire_name() != mode_name {
            return Err(VoiceError::EncryptionModeMismatch);
        }
        tracing::info!(
            name: "discord.transport.encryption_negotiated",
            target: "remedia_discord::connection",
            voice_connection_id = connection_id,
            mode = mode.wire_name(),
            "voice transport encryption mode negotiated"
        );

        let dave_span = tracing::info_span!(
            target: "remedia_discord::connection",
            "discord.dave.initialize",
            voice_connection_id = connection_id,
            protocol_version = dave_version,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let dave_init = dave_span.in_scope(|| {
            let mut dave = DaveProtocol::new(dave_version, info.user_id, info.channel_id)?;
            let key_package = dave.create_key_package()?;
            Ok::<_, VoiceError>((dave, key_package))
        });
        let (dave, initial_key_package) = match dave_init {
            Ok(initialized) => {
                dave_span.record("otel.status_code", "OK");
                initialized
            }
            Err(error) => {
                dave_span.record("otel.status_code", "ERROR");
                dave_span.record("error.type", error.kind());
                tracing::error!(
                    name: "discord.dave.initialization_failed",
                    target: "remedia_discord::connection",
                    voice_connection_id = connection_id,
                    error_kind = error.kind(),
                    "DAVE session initialization failed"
                );
                return Err(error);
            }
        };
        if let Err(error) =
            send_ws_message(&mut websocket, Message::Binary(initial_key_package.into()))
                .instrument(dave_span.clone())
                .await
        {
            dave_span.record("otel.status_code", "ERROR");
            dave_span.record("error.type", error.kind());
            tracing::error!(
                name: "discord.dave.initialization_failed",
                target: "remedia_discord::connection",
                voice_connection_id = connection_id,
                error_kind = error.kind(),
                "DAVE key package could not be sent"
            );
            return Err(error);
        }
        dave_span.record("otel.status_code", "OK");
        tracing::info!(
            name: "discord.dave.initialized",
            target: "remedia_discord::connection",
            voice_connection_id = connection_id,
            protocol_version = dave_version,
            "DAVE session initialized; awaiting MLS readiness"
        );
        let dave = Arc::new(Mutex::new(dave));

        // Some gateways can send DAVE events close to session establishment.
        // Keep their order and replay them through the same state machine.
        let pending_gateway_messages = early_messages;

        let socket_addr = SocketAddr::new(ready.ip, ready.port);
        let rtp = RtpSender::new(ready.ssrc, random(), random(), random(), mode, &secret_key)?;

        let (packet_tx, packet_rx) = flume::bounded(QUEUE_CAPACITY);
        let (control_tx, control_rx) = flume::bounded(16);
        let (generation_tx, generation_rx) = watch::channel(0);
        let cancelled_generation = Arc::new(AtomicU64::new(u64::MAX));
        let (state, _) = watch::channel(ConnectionState::NegotiatingDave);
        let cancellation = TokioCancellationToken::new();
        let failure = Arc::new(Mutex::new(None));
        let actor_failure = Arc::clone(&failure);
        let actor_state = state.clone();
        let actor_cancel = cancellation.clone();
        let actor_cancelled_generation = Arc::clone(&cancelled_generation);
        let actor_id = connection_id;
        let actor_guild_id = info.guild_id;
        let actor_metrics = metrics.clone();
        let actor_span = with_dispatch(&observability, || {
            tracing::info_span!(
                target: "remedia_discord::connection",
                "discord.connection.worker",
                voice_connection_id = actor_id,
                guild_id = actor_guild_id,
                otel.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            )
        });
        let actor_future = async move {
            let result = voice_actor(VoiceActorSetup {
                voice_connection_id: actor_id,
                metrics: actor_metrics.clone(),
                active_guard: ActiveVoiceConnection::new(actor_metrics),
                websocket,
                udp,
                udp_server: socket_addr,
                info,
                ssrc: ready.ssrc,
                heartbeat_interval: hello_interval,
                pending_gateway_messages,
                sequence_ack,
                dave,
                rtp,
                packet_rx,
                control_rx,
                generation_rx,
                cancelled_generation: actor_cancelled_generation,
                cancellation: actor_cancel,
                state: actor_state.clone(),
            })
            .await;
            if let Err(error) = result {
                tracing::Span::current().record("otel.status_code", "ERROR");
                tracing::Span::current().record("error.type", error.kind());
                tracing::error!(
                    name: "discord.connection.worker_failed",
                    target: "remedia_discord::connection",
                    voice_connection_id = actor_id,
                    error_kind = error.kind(),
                    "Discord voice connection worker failed"
                );
                *lock(&actor_failure) = Some(Arc::new(error));
                set_connection_state(
                    &actor_state,
                    ConnectionState::Failed,
                    actor_id,
                    actor_guild_id,
                );
            } else {
                tracing::Span::current().record("otel.status_code", "OK");
                if actor_state.borrow().eq(&ConnectionState::Connected) {
                    set_connection_state(
                        &actor_state,
                        ConnectionState::Disconnected,
                        actor_id,
                        actor_guild_id,
                    );
                }
            }
        }
        .instrument(actor_span);
        let actor_dispatch = observability.dispatch().cloned();
        let worker = match actor_dispatch {
            Some(dispatch) => tokio::spawn(actor_future.with_subscriber(dispatch)),
            None => tokio::spawn(actor_future),
        };

        let connection = Self {
            inner: Arc::new(ConnectionInner {
                voice_connection_id: connection_id,
                guild_id,
                observability: observability.clone(),
                state,
                failure,
                packet_tx,
                control_tx,
                generation: AtomicU64::new(0),
                generation_update: Mutex::new(()),
                generation_tx,
                cancelled_generation,
                accepted_packet_id: AtomicU64::new(0),
                next_packet_id: AtomicU64::new(0),
                cancellation,
                worker: Mutex::new(Some(worker)),
            }),
        };
        let mut state_rx = connection.inner.state.subscribe();
        let readiness = time::timeout(Duration::from_secs(20), async {
            loop {
                match *state_rx.borrow_and_update() {
                    ConnectionState::Connected => return Ok(()),
                    ConnectionState::Failed => {
                        return Err(VoiceError::Worker(connection.failure().map_or_else(
                            || "DAVE setup failed".into(),
                            |error| error.to_string(),
                        )));
                    }
                    ConnectionState::Disconnected => return Err(VoiceError::Disconnected),
                    ConnectionState::NegotiatingDave
                    | ConnectionState::Reconnecting
                    | ConnectionState::Disconnecting => {}
                }
                state_rx
                    .changed()
                    .await
                    .map_err(|_| VoiceError::Disconnected)?;
            }
        })
        .await;
        match readiness {
            Ok(Ok(())) => {
                let establishment_seconds = started_at.elapsed().as_secs_f64();
                metrics.voice_connection_established(establishment_seconds);
                tracing::info!(
                    name: "discord.connection.established",
                    target: "remedia_discord::connection",
                    voice_connection_id = connection_id,
                    establishment_seconds,
                    "Discord voice transport established with DAVE ready"
                );
                Ok(connection)
            }
            Ok(Err(error)) => {
                connection.inner.cancellation.cancel();
                connection.join_worker().await;
                Err(error)
            }
            Err(_) => {
                connection.inner.cancellation.cancel();
                connection.join_worker().await;
                Err(VoiceError::Timeout("waiting for mandatory DAVE setup"))
            }
        }
    }

    /// Read the current transport state.
    pub fn state(&self) -> ConnectionState {
        *self.inner.state.borrow()
    }

    /// Return this connection's internal, process-unique diagnostic correlation ID.
    /// This is unrelated to Discord's voice session ID.
    pub fn voice_connection_id(&self) -> u64 {
        self.inner.voice_connection_id
    }

    pub(crate) fn guild_id(&self) -> u64 {
        self.inner.guild_id
    }

    /// Return the latest transport failure, if the connection failed.
    pub fn failure(&self) -> Option<Arc<VoiceError>> {
        lock(&self.inner.failure).clone()
    }

    /// Create a bounded, synchronous Opus sink for one playback output.
    ///
    /// Creating a new sender supersedes the previous sender on this
    /// connection. Cancelling or finishing this sender leaves the voice
    /// connection open. RTP sequence, timestamp, and nonce state remain owned
    /// by the connection across output instances.
    pub fn opus_sender(&self) -> DiscordOpusSender {
        let _generation_update = lock(&self.inner.generation_update);
        let generation = self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner.generation_tx.send_replace(generation);
        with_dispatch(&self.inner.observability, || {
            tracing::info!(
                name: "discord.rtp_sender.started",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                generation,
                "playback-scoped Opus sender started"
            );
        });
        DiscordOpusSender {
            inner: Arc::clone(&self.inner),
            generation: AtomicU64::new(generation),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    /// Close the transport worker and wait for its sockets and DAVE state to be
    /// released. This operation is idempotent.
    pub async fn disconnect(&self) -> Result<(), VoiceError> {
        let dispatch = self.inner.observability.dispatch().cloned();
        let run = with_dispatch(&self.inner.observability, || self.disconnect_inner());
        match dispatch {
            Some(dispatch) => run.with_subscriber(dispatch).await,
            None => run.await,
        }
    }

    #[tracing::instrument(
        name = "discord.connection.disconnect",
        target = "remedia_discord::connection",
        skip(self),
        fields(
            voice_connection_id = self.inner.voice_connection_id,
            guild_id = self.inner.guild_id
        )
    )]
    async fn disconnect_inner(&self) -> Result<(), VoiceError> {
        if self.state() == ConnectionState::Disconnected {
            self.join_worker().await;
            return Ok(());
        }
        if self.state() == ConnectionState::Failed {
            self.inner.cancellation.cancel();
            self.join_worker().await;
            return Ok(());
        }

        with_dispatch(&self.inner.observability, || {
            set_connection_state(
                &self.inner.state,
                ConnectionState::Disconnecting,
                self.inner.voice_connection_id,
                self.inner.guild_id,
            );
            tracing::info!(
                name: "discord.connection.disconnect_requested",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                "voice transport disconnection requested"
            );
        });
        self.inner.cancellation.cancel();
        self.join_worker().await;
        Ok(())
    }

    async fn join_worker(&self) {
        let worker = lock(&self.inner.worker).take();
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                *lock(&self.inner.failure) = Some(Arc::new(VoiceError::Worker(error.to_string())));
                with_dispatch(&self.inner.observability, || {
                    set_connection_state(
                        &self.inner.state,
                        ConnectionState::Failed,
                        self.inner.voice_connection_id,
                        self.inner.guild_id,
                    );
                });
            }
            return;
        }

        let mut state = self.inner.state.subscribe();
        loop {
            if matches!(
                *state.borrow_and_update(),
                ConnectionState::Disconnected | ConnectionState::Failed
            ) {
                return;
            }
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A playback-scoped synchronous sink that feeds the connection's bounded
/// real-time packet queue. It implements `remedia_core::OpusPacketSink`.
pub struct DiscordOpusSender {
    inner: Arc<ConnectionInner>,
    generation: AtomicU64,
    cancelled: AtomicBool,
    finished: AtomicBool,
}

impl DiscordOpusSender {
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn voice_connection(&self) -> DiscordVoiceConnection {
        DiscordVoiceConnection {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Read the connection state associated with this playback-scoped sender.
    pub fn state(&self) -> ConnectionState {
        *self.inner.state.borrow()
    }

    pub(crate) fn supports_passthrough_packet(packet: &EncodedOpusPacket) -> bool {
        // Leave room for the RTP header and DAVE/transport encryption overhead.
        // The RTP sender still validates the final encrypted datagram size.
        let maximum_opus_payload = MAX_UDP_PACKET_BYTES.saturating_sub(RTP_HEADER_BYTES + 64);
        validate_opus_packet(packet).is_ok() && packet.bytes.len() <= maximum_opus_payload
    }

    fn ensure_current(&self, cancellation: &CoreCancellationToken) -> Result<u64, VoiceError> {
        if cancellation.is_cancelled() || self.inner.cancellation.is_cancelled() {
            return Err(VoiceError::Cancelled);
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err(VoiceError::Cancelled);
        }
        if self.finished.load(Ordering::Acquire) {
            return Err(VoiceError::OutputFinished);
        }
        let generation = self.generation.load(Ordering::Acquire);
        if generation != self.inner.generation.load(Ordering::Acquire) {
            return Err(VoiceError::Superseded);
        }
        match *self.inner.state.borrow() {
            ConnectionState::Connected | ConnectionState::Reconnecting => {}
            ConnectionState::Failed => return Err(VoiceError::Worker(self.failure_message())),
            ConnectionState::NegotiatingDave
            | ConnectionState::Disconnecting
            | ConnectionState::Disconnected => return Err(VoiceError::Disconnected),
        }
        Ok(generation)
    }

    fn failure_message(&self) -> String {
        self.inner
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or_else(|| "voice transport failed".into(), ToString::to_string)
    }

    fn wait_for_control(
        &self,
        receiver: mpsc::Receiver<Result<(), VoiceError>>,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), VoiceError> {
        loop {
            if cancellation.is_cancelled() || self.inner.cancellation.is_cancelled() {
                return Err(VoiceError::Cancelled);
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // The actor owns the transport state. This short wait is
                    // bounded and interruptible rather than a polling loop.
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(VoiceError::Disconnected);
                }
            }
        }
    }

    fn request_barrier(
        &self,
        make_control: impl FnOnce(mpsc::SyncSender<Result<(), VoiceError>>) -> Control,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), VoiceError> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        let control = make_control(ack_tx);
        self.inner
            .control_tx
            .send_timeout(control, CONTROL_SEND_TIMEOUT)
            .map_err(|error| match error {
                flume::SendTimeoutError::Timeout(_) => {
                    VoiceError::Timeout("queueing output control")
                }
                flume::SendTimeoutError::Disconnected(_) => VoiceError::Disconnected,
            })?;
        self.wait_for_control(ack_rx, cancellation)
    }
}

impl OpusPacketSink for DiscordOpusSender {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), OutputError> {
        validate_opus_packet(&packet).map_err(OutputError::with_source)?;
        let generation = self
            .ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        let id = self
            .inner
            .next_packet_id
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let mut queued = QueuedPacket {
            generation,
            id,
            packet,
        };
        loop {
            if cancellation.is_cancelled() || self.cancelled.load(Ordering::Acquire) {
                return Err(OutputError::Cancelled);
            }
            if self.inner.generation.load(Ordering::Acquire) != generation {
                return Err(OutputError::with_source(VoiceError::Superseded));
            }
            match self
                .inner
                .packet_tx
                .send_timeout(queued, Duration::from_millis(25))
            {
                Ok(()) => {
                    self.inner
                        .accepted_packet_id
                        .fetch_max(id, Ordering::AcqRel);
                    return Ok(());
                }
                Err(flume::SendTimeoutError::Timeout(returned)) => queued = returned,
                Err(flume::SendTimeoutError::Disconnected(_)) => {
                    return Err(OutputError::with_source(VoiceError::Disconnected));
                }
            }
        }
    }

    fn supports_passthrough_packet(&self, packet: &EncodedOpusPacket) -> bool {
        Self::supports_passthrough_packet(packet)
    }

    fn flush(
        &self,
        _position: Duration,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        let generation = {
            let _generation_update = lock(&self.inner.generation_update);
            let old_generation = self.generation.load(Ordering::Acquire);
            let generation = self
                .inner
                .generation
                .compare_exchange(
                    old_generation,
                    old_generation.wrapping_add(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map_err(|_| OutputError::with_source(VoiceError::Superseded))?
                .wrapping_add(1);
            self.generation.store(generation, Ordering::Release);
            self.inner.generation_tx.send_replace(generation);
            generation
        };
        with_dispatch(&self.inner.observability, || {
            tracing::info!(
                name: "discord.rtp_sender.flushed",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                guild_id = self.inner.guild_id,
                generation,
                "queued pre-seek RTP frames invalidated"
            );
        });
        self.request_barrier(|ack| Control::Flush { generation, ack }, cancellation)
            .map_err(OutputError::with_source)
    }

    fn pause(&self, cancellation: &CoreCancellationToken) -> Result<(), OutputError> {
        self.ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        let target_id = self.inner.accepted_packet_id.load(Ordering::Acquire);
        self.request_barrier(|ack| Control::Pause { target_id, ack }, cancellation)
            .map_err(OutputError::with_source)
    }

    fn resume(&self, cancellation: &CoreCancellationToken) -> Result<(), OutputError> {
        self.ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        self.request_barrier(|ack| Control::Resume { ack }, cancellation)
            .map_err(OutputError::with_source)
    }

    fn finish(&self, cancellation: &CoreCancellationToken) -> Result<(), OutputError> {
        self.ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        let target_id = self.inner.accepted_packet_id.load(Ordering::Acquire);
        self.request_barrier(|ack| Control::Finish { target_id, ack }, cancellation)
            .map_err(OutputError::with_source)?;
        self.finished.store(true, Ordering::Release);
        with_dispatch(&self.inner.observability, || {
            tracing::info!(
                name: "discord.rtp_sender.finished",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                "playback-scoped RTP sender finished"
            );
        });
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _generation_update = lock(&self.inner.generation_update);
        let old_generation = self.generation.load(Ordering::Acquire);
        if let Ok(current) = self.inner.generation.compare_exchange(
            old_generation,
            old_generation.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            let generation = current.wrapping_add(1);
            self.inner
                .cancelled_generation
                .store(generation, Ordering::Release);
            self.inner.generation_tx.send_replace(generation);
            let _ = self.inner.control_tx.try_send(Control::Cancel {
                generation: old_generation,
            });
        }
        with_dispatch(&self.inner.observability, || {
            tracing::debug!(
                name: "discord.rtp_sender.cancelled",
                target: "remedia_discord::connection",
                voice_connection_id = self.inner.voice_connection_id,
                "playback-scoped RTP sender cancelled"
            );
        });
    }
}

impl Drop for DiscordOpusSender {
    fn drop(&mut self) {
        self.cancel();
    }
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

fn voice_endpoint(endpoint: &str) -> Result<Url, VoiceError> {
    let endpoint = if endpoint.starts_with("wss://") || endpoint.starts_with("ws://") {
        endpoint.to_owned()
    } else {
        format!("wss://{endpoint}")
    };
    let mut url = Url::parse(&endpoint)?;
    if url.scheme() != "wss" || url.host_str().is_none() {
        return Err(VoiceError::InvalidEndpoint);
    }
    url.set_path("/");
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("v", &GATEWAY_VERSION.to_string())
        .append_pair("encoding", "json");
    Ok(url)
}

struct VoiceReady {
    ssrc: u32,
    ip: IpAddr,
    port: u16,
    modes: Vec<String>,
}

async fn establish_voice_session(
    endpoint: &Url,
    info: &VoiceConnectionInfo,
    voice_connection_id: u64,
) -> Result<
    (
        VoiceWebSocket,
        Duration,
        VoiceReady,
        Vec<GatewayMessage>,
        Value,
        UdpSocket,
        Option<u16>,
    ),
    VoiceError,
> {
    tracing::info!(
        name: "discord.voice.websocket_connecting",
        target: "remedia_discord::connection",
        voice_connection_id,
        gateway_version = GATEWAY_VERSION,
        "connecting to Discord voice gateway"
    );
    let websocket_started = Instant::now();
    let (mut websocket, _) = time::timeout(CONNECT_TIMEOUT, connect_async(endpoint.as_str()))
        .await
        .map_err(|_| VoiceError::Timeout("connecting to the voice gateway"))??;
    tracing::info!(
        name: "discord.voice.websocket_connected",
        target: "remedia_discord::connection",
        voice_connection_id,
        elapsed_seconds = websocket_started.elapsed().as_secs_f64(),
        "voice gateway websocket connected"
    );
    send_json(
        &mut websocket,
        &json_message(
            0,
            json!({
                "server_id": info.guild_id.to_string(),
                "user_id": info.user_id.to_string(),
                "session_id": info.session_id,
                "token": info.token,
                "max_dave_protocol_version": DAVE_PROTOCOL_VERSION,
            }),
        ),
    )
    .await?;

    let mut hello_interval = None;
    let mut ready = None;
    let mut early_messages = Vec::new();
    let mut sequence_ack = None;
    while hello_interval.is_none() || ready.is_none() {
        let message = time::timeout(HANDSHAKE_TIMEOUT, websocket.next())
            .await
            .map_err(|_| VoiceError::Timeout("waiting for voice Ready and Hello"))?
            .ok_or_else(|| {
                VoiceError::Protocol("voice gateway closed during handshake".into())
            })??;
        match message {
            Message::Ping(payload) => {
                send_ws_message(&mut websocket, Message::Pong(payload)).await?
            }
            Message::Close(_) => {
                return Err(VoiceError::Protocol(
                    "voice gateway closed during handshake".into(),
                ));
            }
            message => {
                let Some(message) = GatewayMessage::from_ws(message)? else {
                    continue;
                };
                if message.sequence().is_some() {
                    sequence_ack = message.sequence();
                }
                match &message {
                    GatewayMessage::Json {
                        opcode: 2, data, ..
                    } => {
                        let parsed = parse_ready(data)?;
                        tracing::info!(
                            name: "discord.voice.ready_received",
                            target: "remedia_discord::connection",
                            voice_connection_id,
                            ssrc = parsed.ssrc,
                            encryption_mode_count = parsed.modes.len(),
                            "voice gateway Ready received"
                        );
                        ready = Some(parsed);
                    }
                    GatewayMessage::Json {
                        opcode: 8, data, ..
                    } => {
                        let interval = data
                            .get("heartbeat_interval")
                            .and_then(Value::as_u64)
                            .filter(|value| (250..=120_000).contains(value))
                            .ok_or_else(|| {
                                VoiceError::Protocol("invalid heartbeat interval".into())
                            })?;
                        hello_interval = Some(Duration::from_millis(interval));
                    }
                    _ if is_dave_message(&message) => early_messages.push(message),
                    _ => {}
                }
            }
        }
    }
    let ready = ready.ok_or_else(|| VoiceError::Protocol("voice Ready was missing".into()))?;
    let heartbeat_interval =
        hello_interval.ok_or_else(|| VoiceError::Protocol("voice Hello was missing".into()))?;

    tracing::info!(
        name: "discord.voice.udp_discovery_started",
        target: "remedia_discord::connection",
        voice_connection_id,
        "starting UDP IP discovery"
    );
    let udp_started = Instant::now();
    let (udp, external) = establish_udp(&ready).await?;
    tracing::info!(
        name: "discord.voice.udp_discovery_completed",
        target: "remedia_discord::connection",
        voice_connection_id,
        elapsed_seconds = udp_started.elapsed().as_secs_f64(),
        address_family = if external.0.is_ipv4() { "ipv4" } else { "ipv6" },
        "UDP IP discovery completed"
    );
    let mode = EncryptionMode::choose(&ready.modes)?;
    select_protocol(&mut websocket, external, mode).await?;
    tracing::debug!(
        name: "discord.voice.protocol_selected",
        target: "remedia_discord::connection",
        voice_connection_id,
        encryption_mode = mode.wire_name(),
        "voice UDP protocol selected"
    );

    let mut session_data = None;
    while session_data.is_none() {
        let message = time::timeout(HANDSHAKE_TIMEOUT, websocket.next())
            .await
            .map_err(|_| VoiceError::Timeout("waiting for the voice session description"))?
            .ok_or_else(|| VoiceError::Protocol("voice gateway closed during setup".into()))??;
        match message {
            Message::Ping(payload) => {
                send_ws_message(&mut websocket, Message::Pong(payload)).await?
            }
            Message::Close(_) => {
                return Err(VoiceError::Protocol(
                    "voice gateway closed during setup".into(),
                ));
            }
            message => {
                let Some(message) = GatewayMessage::from_ws(message)? else {
                    continue;
                };
                if message.sequence().is_some() {
                    sequence_ack = message.sequence();
                }
                match &message {
                    GatewayMessage::Json {
                        opcode: 4, data, ..
                    } => {
                        session_data = Some(data.clone());
                    }
                    _ if is_dave_message(&message) => {
                        if early_messages.len() >= 64 {
                            return Err(VoiceError::Protocol(
                                "too many DAVE events arrived before session setup".into(),
                            ));
                        }
                        early_messages.push(message);
                    }
                    _ => {}
                }
            }
        }
    }

    let session_data = session_data
        .ok_or_else(|| VoiceError::Protocol("voice session description was missing".into()))?;
    Ok((
        websocket,
        heartbeat_interval,
        ready,
        early_messages,
        session_data,
        udp,
        sequence_ack,
    ))
}

fn parse_ready(data: &Value) -> Result<VoiceReady, VoiceError> {
    let ssrc = u32::try_from(
        data.get("ssrc")
            .and_then(Value::as_u64)
            .ok_or_else(|| VoiceError::Protocol("voice Ready omitted SSRC".into()))?,
    )
    .map_err(|_| VoiceError::Protocol("voice Ready SSRC exceeded u32".into()))?;
    let ip = data
        .get("ip")
        .and_then(Value::as_str)
        .ok_or_else(|| VoiceError::Protocol("voice Ready omitted UDP IP".into()))?
        .parse::<IpAddr>()
        .map_err(|_| VoiceError::Protocol("voice Ready UDP IP was invalid".into()))?;
    let port = data
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| VoiceError::Protocol("voice Ready UDP port was invalid".into()))?;
    let modes = data
        .get("modes")
        .and_then(Value::as_array)
        .ok_or_else(|| VoiceError::Protocol("voice Ready omitted encryption modes".into()))?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    Ok(VoiceReady {
        ssrc,
        ip,
        port,
        modes,
    })
}

fn parse_secret_key(data: &Value) -> Result<Vec<u8>, VoiceError> {
    let values = data
        .get("secret_key")
        .and_then(Value::as_array)
        .ok_or_else(|| VoiceError::Protocol("session description omitted secret key".into()))?;
    if values.len() != 32 {
        return Err(VoiceError::InvalidSessionKey);
    }
    values
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u8::try_from(number).ok())
                .ok_or_else(|| VoiceError::Protocol("session key contained invalid byte".into()))
        })
        .collect()
}

async fn establish_udp(ready: &VoiceReady) -> Result<(UdpSocket, (IpAddr, u16)), VoiceError> {
    let bind_addr = match ready.ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let udp = UdpSocket::bind(bind_addr).await?;
    let server = SocketAddr::new(ready.ip, ready.port);
    time::timeout(
        Duration::from_secs(2),
        udp.send_to(&ip_discovery_request(ready.ssrc), server),
    )
    .await
    .map_err(|_| VoiceError::Timeout("sending UDP IP discovery"))??;
    let mut response = [0; 256];
    let (received, source) = time::timeout(UDP_DISCOVERY_TIMEOUT, udp.recv_from(&mut response))
        .await
        .map_err(|_| VoiceError::Timeout("performing UDP IP discovery"))??;
    if source != server {
        return Err(VoiceError::Protocol(
            "UDP discovery response came from an unexpected endpoint".into(),
        ));
    }
    let external = parse_ip_discovery_response(&response[..received], ready.ssrc)?;
    Ok((udp, external))
}

async fn select_protocol(
    websocket: &mut VoiceWebSocket,
    external: (IpAddr, u16),
    mode: EncryptionMode,
) -> Result<(), VoiceError> {
    send_json(
        websocket,
        &json_message(
            1,
            json!({
                "protocol": "udp",
                "data": {
                    "address": external.0.to_string(),
                    "port": external.1,
                    "mode": mode.wire_name(),
                }
            }),
        ),
    )
    .await
}

async fn send_json(websocket: &mut VoiceWebSocket, value: &Value) -> Result<(), VoiceError> {
    send_ws_message(websocket, Message::Text(value.to_string().into())).await
}

async fn send_ws_message(
    websocket: &mut VoiceWebSocket,
    message: Message,
) -> Result<(), VoiceError> {
    time::timeout(Duration::from_secs(5), websocket.send(message))
        .await
        .map_err(|_| VoiceError::Timeout("sending on the voice WebSocket"))??;
    Ok(())
}

fn is_dave_message(message: &GatewayMessage) -> bool {
    match message {
        GatewayMessage::Json { opcode, .. } => matches!(*opcode, 11 | 13 | 21 | 22 | 24 | 31),
        GatewayMessage::Binary { opcode, .. } => matches!(*opcode, 25 | 27 | 29 | 30),
    }
}

fn gateway_opcode(message: &GatewayMessage) -> u64 {
    match message {
        GatewayMessage::Json { opcode, .. } => *opcode,
        GatewayMessage::Binary { opcode, .. } => u64::from(*opcode),
    }
}

struct VoiceActorSetup {
    voice_connection_id: u64,
    metrics: remedia_observability::RemediaMetrics,
    active_guard: ActiveVoiceConnection,
    websocket: VoiceWebSocket,
    udp: UdpSocket,
    udp_server: SocketAddr,
    info: VoiceConnectionInfo,
    ssrc: u32,
    heartbeat_interval: Duration,
    pending_gateway_messages: Vec<GatewayMessage>,
    sequence_ack: Option<u16>,
    dave: Arc<Mutex<DaveProtocol>>,
    rtp: RtpSender,
    packet_rx: Receiver<QueuedPacket>,
    control_rx: Receiver<Control>,
    generation_rx: watch::Receiver<u64>,
    cancelled_generation: Arc<AtomicU64>,
    cancellation: TokioCancellationToken,
    state: watch::Sender<ConnectionState>,
}

async fn voice_actor(setup: VoiceActorSetup) -> Result<(), VoiceError> {
    let VoiceActorSetup {
        voice_connection_id,
        metrics,
        mut active_guard,
        mut websocket,
        udp,
        udp_server,
        info,
        ssrc,
        heartbeat_interval,
        mut pending_gateway_messages,
        mut sequence_ack,
        dave,
        mut rtp,
        packet_rx,
        control_rx,
        mut generation_rx,
        cancelled_generation,
        cancellation,
        state,
    } = setup;
    let packet_observability = RtpSendObservability {
        voice_connection_id,
        metrics: metrics.clone(),
    };
    let mut active_generation = *generation_rx.borrow();
    let mut queued = VecDeque::with_capacity(QUEUE_CAPACITY);
    let mut processed_id = 0_u64;
    let mut barrier: Option<PendingBarrier> = None;
    let mut paused = false;
    let mut playback_active = false;
    let mut speaking = false;
    let mut next_send_at: Option<Instant> = None;
    let mut heartbeat_nonce = 0_u64;
    let mut pending_heartbeat_nonce = None;
    let mut last_heartbeat_ack = Instant::now();
    let mut heartbeat = time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

    for message in pending_gateway_messages.drain(..) {
        process_gateway_message(&mut websocket, &dave, message, voice_connection_id).await?;
        if lock(&dave).is_ready() {
            active_guard.mark_connected();
            set_connection_state(
                &state,
                ConnectionState::Connected,
                voice_connection_id,
                info.guild_id,
            );
        }
    }

    loop {
        let deadline = next_send_at;
        enum Event {
            Socket(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
            Packet(Result<QueuedPacket, flume::RecvError>),
            Control(Result<Control, flume::RecvError>),
            Heartbeat,
            Deadline,
            Generation(Result<(), watch::error::RecvError>),
            Cancel,
        }
        let dave_ready = lock(&dave).is_ready();
        let event = tokio::select! {
            _ = cancellation.cancelled() => Event::Cancel,
            message = websocket.next() => Event::Socket(message),
            packet = packet_rx.recv_async(), if queued.len() < QUEUE_CAPACITY && dave_ready => Event::Packet(packet),
            control = control_rx.recv_async() => Event::Control(control),
            _ = heartbeat.tick() => Event::Heartbeat,
            _ = async { if let Some(deadline) = deadline { time::sleep_until(deadline).await } }, if deadline.is_some() => Event::Deadline,
            changed = generation_rx.changed() => Event::Generation(changed),
        };
        let close_code = match &event {
            Event::Socket(Some(Ok(Message::Close(Some(frame))))) => Some(u16::from(frame.code)),
            _ => None,
        };

        match event {
            Event::Cancel => {
                let _ = send_speaking(&mut websocket, ssrc, false, voice_connection_id).await;
                set_connection_state(
                    &state,
                    ConnectionState::Disconnected,
                    voice_connection_id,
                    info.guild_id,
                );
                tracing::info!(
                    name: "discord.connection.disconnected",
                    target: "remedia_discord::connection",
                    voice_connection_id,
                    "voice transport worker stopped"
                );
                return Ok(());
            }
            Event::Socket(None) | Event::Socket(Some(Ok(Message::Close(_)))) => {
                match close_code
                    .map(voice_close_policy)
                    .unwrap_or(VoiceClosePolicy::Resume)
                {
                    VoiceClosePolicy::WaitForGateway => {
                        set_connection_state(
                            &state,
                            ConnectionState::Disconnected,
                            voice_connection_id,
                            info.guild_id,
                        );
                        tracing::warn!(
                            name: "discord.connection.closed_for_gateway_update",
                            target: "remedia_discord::connection",
                            voice_connection_id,
                            guild_id = info.guild_id,
                            close_code,
                            "voice gateway closed this session; waiting for fresh Gateway credentials"
                        );
                        return Ok(());
                    }
                    VoiceClosePolicy::FreshIdentify => {
                        tracing::warn!(
                            name: "discord.connection.resume_not_permitted",
                            target: "remedia_discord::connection",
                            voice_connection_id,
                            guild_id = info.guild_id,
                            close_code,
                            "voice session cannot be resumed; a fresh Gateway voice handshake is required"
                        );
                        return Err(VoiceError::ResumeFailed(format!(
                            "voice session is no longer resumable (close code {})",
                            close_code.unwrap_or_default()
                        )));
                    }
                    VoiceClosePolicy::Resume => {}
                }
                metrics.voice_reconnect_attempt();
                set_connection_state(
                    &state,
                    ConnectionState::Reconnecting,
                    voice_connection_id,
                    info.guild_id,
                );
                tracing::warn!(
                    name: "discord.connection.reconnect_started",
                    target: "remedia_discord::connection",
                    voice_connection_id,
                    guild_id = info.guild_id,
                    close_code,
                    reason = "gateway_closed",
                    "voice gateway connection closed; attempting resume"
                );
                let Some(resumed) = resume_for_actor(&info, sequence_ack, &cancellation).await?
                else {
                    set_connection_state(
                        &state,
                        ConnectionState::Disconnected,
                        voice_connection_id,
                        info.guild_id,
                    );
                    return Ok(());
                };
                websocket = resumed;
                let resumed_state = if lock(&dave).is_ready() {
                    ConnectionState::Connected
                } else {
                    ConnectionState::NegotiatingDave
                };
                if resumed_state == ConnectionState::Connected {
                    active_guard.mark_connected();
                }
                set_connection_state(&state, resumed_state, voice_connection_id, info.guild_id);
                last_heartbeat_ack = Instant::now();
                pending_heartbeat_nonce = None;
            }
            Event::Socket(Some(Err(_))) => {
                metrics.voice_reconnect_attempt();
                set_connection_state(
                    &state,
                    ConnectionState::Reconnecting,
                    voice_connection_id,
                    info.guild_id,
                );
                tracing::warn!(
                    name: "discord.connection.reconnect_started",
                    target: "remedia_discord::connection",
                    voice_connection_id,
                    guild_id = info.guild_id,
                    reason = "websocket_error",
                    "voice gateway websocket failed; attempting resume"
                );
                let Some(resumed) = resume_for_actor(&info, sequence_ack, &cancellation).await?
                else {
                    set_connection_state(
                        &state,
                        ConnectionState::Disconnected,
                        voice_connection_id,
                        info.guild_id,
                    );
                    return Ok(());
                };
                websocket = resumed;
                let resumed_state = if lock(&dave).is_ready() {
                    ConnectionState::Connected
                } else {
                    ConnectionState::NegotiatingDave
                };
                if resumed_state == ConnectionState::Connected {
                    active_guard.mark_connected();
                }
                set_connection_state(&state, resumed_state, voice_connection_id, info.guild_id);
                last_heartbeat_ack = Instant::now();
                pending_heartbeat_nonce = None;
            }
            Event::Socket(Some(Ok(Message::Ping(payload)))) => {
                send_ws_message(&mut websocket, Message::Pong(payload)).await?;
            }
            Event::Socket(Some(Ok(message))) => {
                if let Some(message) = GatewayMessage::from_ws(message)? {
                    if let Some(sequence) = message.sequence() {
                        sequence_ack = Some(sequence);
                    }
                    if let GatewayMessage::Json {
                        opcode: 6, data, ..
                    } = &message
                    {
                        if heartbeat_ack_matches(
                            pending_heartbeat_nonce,
                            data.get("t").and_then(Value::as_u64),
                        ) {
                            pending_heartbeat_nonce = None;
                            last_heartbeat_ack = Instant::now();
                        }
                    } else if matches!(message, GatewayMessage::Json { opcode: 9, .. }) {
                        // Opcode 9 is a successful Resume and carries null data.
                    } else {
                        process_gateway_message(
                            &mut websocket,
                            &dave,
                            message,
                            voice_connection_id,
                        )
                        .await?;
                        if lock(&dave).is_ready() {
                            active_guard.mark_connected();
                            set_connection_state(
                                &state,
                                ConnectionState::Connected,
                                voice_connection_id,
                                info.guild_id,
                            );
                        }
                    }
                }
            }
            Event::Packet(Ok(packet)) => {
                let published_generation = *generation_rx.borrow();
                if synchronize_output_generation(
                    published_generation,
                    &mut active_generation,
                    &cancelled_generation,
                ) {
                    begin_output_cancel(
                        &mut queued,
                        &mut processed_id,
                        &mut barrier,
                        &mut paused,
                        &mut playback_active,
                        speaking,
                        &mut next_send_at,
                    );
                }
                if packet.generation == active_generation {
                    if supersede_cancel_barrier(
                        &mut barrier,
                        &mut paused,
                        &mut playback_active,
                        &mut next_send_at,
                    ) {
                        tracing::debug!(
                            name: "discord.rtp_sender.cancel_superseded",
                            target: "remedia_discord::connection",
                            voice_connection_id,
                            generation = active_generation,
                            "new playback superseded the previous sender cancellation barrier"
                        );
                    }
                    playback_active = true;
                    queued.push_back(packet);
                    if next_send_at.is_none() && !paused {
                        next_send_at = Some(Instant::now());
                    }
                } else {
                    metrics.rtp_packet_dropped();
                    tracing::trace!(
                        name: "discord.rtp.packet_dropped",
                        target: "remedia_discord::connection",
                        voice_connection_id,
                        reason = "stale_generation",
                        samples_per_channel = packet.packet.samples_per_channel,
                        "queued RTP packet discarded after output generation changed"
                    );
                    processed_id = processed_id.max(packet.id);
                }
            }
            Event::Packet(Err(_)) => return Err(VoiceError::Disconnected),
            Event::Control(Ok(Control::Flush { generation, ack })) => {
                active_generation = generation;
                metrics.rtp_packet_dropped_by(u64::try_from(queued.len()).unwrap_or(u64::MAX));
                while let Some(packet) = queued.pop_front() {
                    processed_id = processed_id.max(packet.id);
                }
                if !paused && playback_active {
                    next_send_at = Some(Instant::now());
                }
                let _ = ack.send(Ok(()));
            }
            Event::Control(Ok(Control::Pause { target_id, ack })) => {
                let silence_packets = if speaking { 5 } else { 0 };
                barrier = Some(PendingBarrier {
                    target_id,
                    silence_packets,
                    kind: BarrierKind::Pause,
                    ack: Some(ack),
                });
                if next_send_at.is_none() {
                    next_send_at = Some(Instant::now());
                }
            }
            Event::Control(Ok(Control::Resume { ack })) => {
                if !paused {
                    let _ = ack.send(Err(VoiceError::Protocol(
                        "output resume was requested while it was not paused".into(),
                    )));
                } else {
                    paused = false;
                    playback_active = true;
                    send_speaking(&mut websocket, ssrc, true, voice_connection_id).await?;
                    speaking = true;
                    next_send_at = None;
                    let _ = ack.send(Ok(()));
                }
            }
            Event::Control(Ok(Control::Finish { target_id, ack })) => {
                let silence_packets = if speaking { 5 } else { 0 };
                barrier = Some(PendingBarrier {
                    target_id,
                    silence_packets,
                    kind: BarrierKind::Finish,
                    ack: Some(ack),
                });
                if next_send_at.is_none() {
                    next_send_at = Some(Instant::now());
                }
            }
            Event::Control(Ok(Control::Cancel { generation })) => {
                // The control queue and watch channel can be observed in a
                // different order. Only honor this cancellation if its
                // generation is still the latest published one; a newer
                // sender supersedes it.
                let published_generation = *generation_rx.borrow();
                if cancellation_matches_generation(
                    generation,
                    published_generation,
                    &cancelled_generation,
                ) && synchronize_output_generation(
                    published_generation,
                    &mut active_generation,
                    &cancelled_generation,
                ) {
                    begin_output_cancel(
                        &mut queued,
                        &mut processed_id,
                        &mut barrier,
                        &mut paused,
                        &mut playback_active,
                        speaking,
                        &mut next_send_at,
                    );
                }
            }
            Event::Control(Err(_)) => return Ok(()),
            Event::Heartbeat => {
                if heartbeat_expired(last_heartbeat_ack, Instant::now(), heartbeat_interval) {
                    metrics.voice_reconnect_attempt();
                    set_connection_state(
                        &state,
                        ConnectionState::Reconnecting,
                        voice_connection_id,
                        info.guild_id,
                    );
                    tracing::warn!(
                        name: "discord.voice.heartbeat_timeout",
                        target: "remedia_discord::connection",
                        voice_connection_id,
                        "voice heartbeat acknowledgement timed out"
                    );
                    let Some(resumed) =
                        resume_for_actor(&info, sequence_ack, &cancellation).await?
                    else {
                        set_connection_state(
                            &state,
                            ConnectionState::Disconnected,
                            voice_connection_id,
                            info.guild_id,
                        );
                        return Ok(());
                    };
                    websocket = resumed;
                    let resumed_state = if lock(&dave).is_ready() {
                        ConnectionState::Connected
                    } else {
                        ConnectionState::NegotiatingDave
                    };
                    if resumed_state == ConnectionState::Connected {
                        active_guard.mark_connected();
                    }
                    set_connection_state(&state, resumed_state, voice_connection_id, info.guild_id);
                    last_heartbeat_ack = Instant::now();
                    pending_heartbeat_nonce = None;
                } else {
                    heartbeat_nonce = heartbeat_nonce.wrapping_add(1);
                    pending_heartbeat_nonce = Some(heartbeat_nonce);
                    send_json(
                        &mut websocket,
                        &json_message(
                            3,
                            json!({
                                "t": heartbeat_nonce,
                                "seq_ack": sequence_ack.map_or(-1_i64, i64::from),
                            }),
                        ),
                    )
                    .await?;
                }
            }
            Event::Deadline => {
                let now = Instant::now();
                let scheduled_deadline = deadline.unwrap_or(now);
                if let Some(pending) = barrier.as_mut()
                    && processed_id >= pending.target_id
                {
                    if pending.silence_packets > 0 {
                        send_transport_packet(
                            &udp,
                            udp_server,
                            &mut rtp,
                            &dave,
                            &packet_observability,
                            now.saturating_duration_since(scheduled_deadline),
                            &EncodedOpusPacket {
                                bytes: SILENCE_PACKET.to_vec(),
                                samples_per_channel: 960,
                                media_samples_per_channel: 960,
                                start_sample: 0,
                            },
                        )
                        .await?;
                        pending.silence_packets -= 1;
                        next_send_at = Some(paced_deadline(
                            scheduled_deadline,
                            now,
                            Duration::from_millis(20),
                        ));
                        continue;
                    }

                    let action = barrier.take().expect("barrier exists");
                    send_speaking(&mut websocket, ssrc, false, voice_connection_id).await?;
                    speaking = false;
                    match action.kind {
                        BarrierKind::Pause => {
                            paused = true;
                            playback_active = false;
                            next_send_at = None;
                        }
                        BarrierKind::Finish | BarrierKind::Cancel => {
                            paused = false;
                            playback_active = false;
                            next_send_at = None;
                        }
                    }
                    if let Some(ack) = action.ack {
                        let _ = ack.send(Ok(()));
                    }
                    continue;
                }

                if paused || !playback_active {
                    next_send_at = None;
                    continue;
                }
                if !lock(&dave).is_ready() {
                    next_send_at = Some(now + Duration::from_millis(20));
                    continue;
                }
                if !speaking {
                    send_speaking(&mut websocket, ssrc, true, voice_connection_id).await?;
                    speaking = true;
                }

                while queued
                    .front()
                    .is_some_and(|packet| packet.generation != active_generation)
                {
                    if let Some(packet) = queued.pop_front() {
                        metrics.rtp_packet_dropped();
                        processed_id = processed_id.max(packet.id);
                        rtp.skip_packet_duration(packet.packet.samples_per_channel);
                    }
                }

                let packet = queued.pop_front();
                if let Some(packet) = packet {
                    let duration = Duration::from_secs_f64(
                        f64::from(packet.packet.samples_per_channel) / f64::from(PACKET_RATE),
                    );
                    match packet_deadline(scheduled_deadline, now, duration) {
                        None => {
                            metrics.rtp_packet_dropped();
                            tracing::trace!(
                                name: "discord.rtp.packet_dropped",
                                target: "remedia_discord::connection",
                                voice_connection_id,
                                reason = "late_deadline",
                                late_by_ms = now.saturating_duration_since(scheduled_deadline).as_secs_f64() * 1_000.0,
                                samples_per_channel = packet.packet.samples_per_channel,
                                "late RTP packet dropped to avoid catch-up bursts"
                            );
                            rtp.skip_packet_duration(packet.packet.samples_per_channel);
                            processed_id = processed_id.max(packet.id);
                            next_send_at = Some(now + duration);
                        }
                        Some(next_deadline) => {
                            send_transport_packet(
                                &udp,
                                udp_server,
                                &mut rtp,
                                &dave,
                                &packet_observability,
                                now.saturating_duration_since(scheduled_deadline),
                                &packet.packet,
                            )
                            .await?;
                            processed_id = processed_id.max(packet.id);
                            next_send_at = Some(next_deadline);
                        }
                    }
                } else {
                    metrics.packet_underrun();
                    tracing::trace!(
                        name: "discord.rtp.packet_underrun",
                        target: "remedia_discord::connection",
                        voice_connection_id,
                        "RTP send deadline reached with no audio packet queued"
                    );
                    let silence = EncodedOpusPacket {
                        bytes: SILENCE_PACKET.to_vec(),
                        samples_per_channel: 960,
                        media_samples_per_channel: 960,
                        start_sample: 0,
                    };
                    send_transport_packet(
                        &udp,
                        udp_server,
                        &mut rtp,
                        &dave,
                        &packet_observability,
                        now.saturating_duration_since(scheduled_deadline),
                        &silence,
                    )
                    .await?;
                    next_send_at = Some(paced_deadline(
                        scheduled_deadline,
                        now,
                        Duration::from_millis(20),
                    ));
                }
            }
            Event::Generation(Ok(())) => {
                let generation = *generation_rx.borrow_and_update();
                if synchronize_output_generation(
                    generation,
                    &mut active_generation,
                    &cancelled_generation,
                ) {
                    begin_output_cancel(
                        &mut queued,
                        &mut processed_id,
                        &mut barrier,
                        &mut paused,
                        &mut playback_active,
                        speaking,
                        &mut next_send_at,
                    );
                }
            }
            Event::Generation(Err(_)) => return Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VoiceClosePolicy {
    /// Resume is valid for an interrupted session, including Discord's
    /// recoverable voice-server-crash close code 4015.
    Resume,
    /// The voice session expired; the Gateway must provide fresh credentials.
    FreshIdentify,
    /// Discord says not to reconnect until a fresh Gateway voice update arrives.
    WaitForGateway,
}

fn voice_close_policy(code: u16) -> VoiceClosePolicy {
    match code {
        4006 | 4009 => VoiceClosePolicy::FreshIdentify,
        4014 | 4021 | 4022 => VoiceClosePolicy::WaitForGateway,
        _ => VoiceClosePolicy::Resume,
    }
}

async fn process_gateway_message(
    websocket: &mut VoiceWebSocket,
    dave: &Arc<Mutex<DaveProtocol>>,
    message: GatewayMessage,
    voice_connection_id: u64,
) -> Result<(), VoiceError> {
    let gateway_opcode = gateway_opcode(&message);
    let (ready_before, epoch_before, outgoing) = {
        let mut dave = lock(dave);
        let ready_before = dave.is_ready();
        let epoch_before = dave.epoch();
        let outgoing = dave.handle(&message);
        (ready_before, epoch_before, outgoing)
    };
    let outgoing = match outgoing {
        Ok(outgoing) => outgoing,
        Err(error) => {
            tracing::error!(
                name: "discord.dave.transition_failed",
                target: "remedia_discord::connection",
                voice_connection_id,
                gateway_opcode,
                error_kind = error.kind(),
                "DAVE gateway transition failed"
            );
            return Err(error);
        }
    };
    let (ready_after, epoch_after, member_count) = {
        let dave = lock(dave);
        (dave.is_ready(), dave.epoch(), dave.member_count())
    };
    tracing::debug!(
        name: "discord.dave.gateway_transition",
        target: "remedia_discord::connection",
        voice_connection_id,
        gateway_opcode,
        epoch_before,
        epoch_after,
        member_count,
        ready_before,
        ready_after,
        "DAVE gateway transition processed"
    );
    if !ready_before && ready_after {
        tracing::info!(
            name: "discord.dave.ready",
            target: "remedia_discord::connection",
            voice_connection_id,
            epoch = epoch_after,
            "DAVE media encryption is ready"
        );
    } else if epoch_before != epoch_after {
        tracing::info!(
            name: "discord.dave.epoch_changed",
            target: "remedia_discord::connection",
            voice_connection_id,
            previous_epoch = epoch_before,
            epoch = epoch_after,
            "DAVE media encryption epoch changed"
        );
    }
    if matches!(gateway_opcode, 11 | 13) {
        tracing::info!(
            name: "discord.dave.membership_changed",
            target: "remedia_discord::connection",
            voice_connection_id,
            member_count,
            "DAVE group membership changed"
        );
    }
    for message in outgoing {
        match message {
            OutgoingGatewayMessage::Json(value) => send_json(websocket, &value).await?,
            OutgoingGatewayMessage::Binary(bytes) => {
                send_ws_message(websocket, Message::Binary(bytes.into())).await?
            }
        }
    }
    Ok(())
}

fn begin_output_cancel(
    queued: &mut VecDeque<QueuedPacket>,
    processed_id: &mut u64,
    barrier: &mut Option<PendingBarrier>,
    paused: &mut bool,
    playback_active: &mut bool,
    speaking: bool,
    next_send_at: &mut Option<Instant>,
) {
    if matches!(
        barrier.as_ref().map(|pending| &pending.kind),
        Some(BarrierKind::Cancel)
    ) {
        return;
    }
    if !*playback_active && !*paused && !speaking && queued.is_empty() {
        return;
    }

    *paused = false;
    while let Some(packet) = queued.pop_front() {
        *processed_id = (*processed_id).max(packet.id);
    }
    *playback_active = false;
    if speaking {
        *barrier = Some(PendingBarrier {
            target_id: *processed_id,
            silence_packets: 5,
            kind: BarrierKind::Cancel,
            ack: None,
        });
        if next_send_at.is_none() {
            *next_send_at = Some(Instant::now());
        }
    } else {
        *barrier = None;
        *next_send_at = None;
    }
}

fn cancellation_matches_generation(
    cancelled_output_generation: u64,
    published_generation: u64,
    cancelled_generation: &AtomicU64,
) -> bool {
    published_generation == cancelled_output_generation.wrapping_add(1)
        && cancelled_generation.load(Ordering::Acquire) == published_generation
}

fn synchronize_output_generation(
    generation: u64,
    active_generation: &mut u64,
    cancelled_generation: &AtomicU64,
) -> bool {
    if generation == *active_generation {
        return false;
    }

    *active_generation = generation;
    cancelled_generation.load(Ordering::Acquire) == generation
}

/// A cancellation barrier belongs to the output generation that was stopped.
/// If a newer generation already has a packet, keep speaking and resume its
/// pacing instead of letting the old barrier deactivate the new output.
fn supersede_cancel_barrier(
    barrier: &mut Option<PendingBarrier>,
    paused: &mut bool,
    playback_active: &mut bool,
    next_send_at: &mut Option<Instant>,
) -> bool {
    if !matches!(
        barrier.as_ref().map(|pending| &pending.kind),
        Some(BarrierKind::Cancel)
    ) {
        return false;
    }

    *barrier = None;
    *paused = false;
    *playback_active = true;
    if next_send_at.is_none() {
        *next_send_at = Some(Instant::now());
    }
    true
}

async fn send_speaking(
    websocket: &mut VoiceWebSocket,
    ssrc: u32,
    speaking: bool,
    voice_connection_id: u64,
) -> Result<(), VoiceError> {
    send_json(
        websocket,
        &json_message(
            5,
            json!({
                "speaking": if speaking { 1 } else { 0 },
                "delay": 0,
                "ssrc": ssrc,
            }),
        ),
    )
    .await
    .map(|()| {
        tracing::info!(
            name: "discord.voice.speaking_changed",
            target: "remedia_discord::connection",
            voice_connection_id,
            speaking,
            "Discord speaking state changed"
        );
    })
}

async fn send_transport_packet(
    udp: &UdpSocket,
    server: SocketAddr,
    rtp: &mut RtpSender,
    dave: &Arc<Mutex<DaveProtocol>>,
    observability: &RtpSendObservability,
    scheduling_delay: Duration,
    packet: &EncodedOpusPacket,
) -> Result<(), VoiceError> {
    let bytes = match rtp.encode_packet(packet, &mut lock(dave)) {
        Ok(bytes) => bytes,
        Err(error) => {
            observability.metrics.rtp_packet_dropped();
            tracing::error!(
                name: "discord.rtp.packet_encryption_failed",
                target: "remedia_discord::connection",
                voice_connection_id = observability.voice_connection_id,
                error_kind = error.kind(),
                "could not encrypt or packetize Opus frame"
            );
            return Err(error);
        }
    };
    let bytes_len = bytes.len();
    let send_result = time::timeout(Duration::from_secs(2), udp.send_to(&bytes, server))
        .await
        .map_err(|_| VoiceError::Timeout("sending an RTP packet"))
        .and_then(|result| result.map_err(VoiceError::from));
    let sent = match send_result {
        Ok(sent) => sent,
        Err(error) => {
            observability.metrics.rtp_packet_dropped();
            tracing::error!(
                name: "discord.rtp.packet_send_failed",
                target: "remedia_discord::connection",
                voice_connection_id = observability.voice_connection_id,
                error_kind = error.kind(),
                "UDP RTP packet send failed"
            );
            return Err(error);
        }
    };
    if sent != bytes_len {
        observability.metrics.rtp_packet_dropped();
        let error = VoiceError::Io(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "UDP socket did not send a complete RTP packet",
        ));
        tracing::error!(
            name: "discord.rtp.packet_send_failed",
            target: "remedia_discord::connection",
            voice_connection_id = observability.voice_connection_id,
            error_kind = error.kind(),
            "UDP RTP packet send was incomplete"
        );
        return Err(error);
    }
    observability.metrics.rtp_packet_sent();
    observability
        .metrics
        .record_scheduling_delay(scheduling_delay.as_secs_f64());
    tracing::trace!(
        name: "discord.rtp.packet_sent",
        target: "remedia_discord::connection",
        voice_connection_id = observability.voice_connection_id,
        samples_per_channel = packet.samples_per_channel,
        packet_bytes = bytes_len,
        scheduling_delay_ms = scheduling_delay.as_secs_f64() * 1_000.0,
        "encrypted RTP packet sent"
    );
    Ok(())
}

/// Preserve the intended packet cadence unless a packet is more than one
/// frame late. A late frame is dropped and the schedule is rebased to avoid a
/// burst of catch-up packets.
fn packet_deadline(scheduled: Instant, now: Instant, duration: Duration) -> Option<Instant> {
    let next = scheduled + duration;
    if now > next { None } else { Some(next) }
}

fn paced_deadline(scheduled: Instant, now: Instant, duration: Duration) -> Instant {
    (scheduled + duration).max(now + duration)
}

fn heartbeat_ack_matches(pending: Option<u64>, received: Option<u64>) -> bool {
    pending.is_some() && pending == received
}

fn heartbeat_expired(last_ack: Instant, now: Instant, interval: Duration) -> bool {
    now.duration_since(last_ack) > interval.saturating_mul(2)
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

async fn resume_gateway(
    info: &VoiceConnectionInfo,
    sequence_ack: Option<u16>,
) -> Result<VoiceWebSocket, VoiceError> {
    let endpoint = voice_endpoint(&info.endpoint)?;
    let (mut websocket, _) = time::timeout(CONNECT_TIMEOUT, connect_async(endpoint.as_str()))
        .await
        .map_err(|_| VoiceError::Timeout("reconnecting to the voice gateway"))??;
    send_json(
        &mut websocket,
        &json_message(
            7,
            json!({
                "server_id": info.guild_id.to_string(),
                "session_id": info.session_id,
                "token": info.token,
                "seq_ack": sequence_ack.map_or(-1_i64, i64::from),
            }),
        ),
    )
    .await?;
    loop {
        let message = time::timeout(HANDSHAKE_TIMEOUT, websocket.next())
            .await
            .map_err(|_| VoiceError::Timeout("waiting for voice Resume"))?
            .ok_or_else(|| {
                VoiceError::ResumeFailed("gateway closed the resumed socket".into())
            })??;
        match message {
            Message::Ping(payload) => {
                send_ws_message(&mut websocket, Message::Pong(payload)).await?
            }
            Message::Close(_) => {
                return Err(VoiceError::ResumeFailed(
                    "gateway closed the resumed socket".into(),
                ));
            }
            message => {
                let Some(message) = GatewayMessage::from_ws(message)? else {
                    continue;
                };
                if let GatewayMessage::Json { opcode: 9, .. } = message {
                    return Ok(websocket);
                }
            }
        }
    }
}

async fn resume_gateway_cancellable(
    info: &VoiceConnectionInfo,
    sequence_ack: Option<u16>,
    cancellation: &TokioCancellationToken,
) -> Result<VoiceWebSocket, VoiceError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(VoiceError::Cancelled),
        result = resume_gateway(info, sequence_ack) => result,
    }
}

async fn resume_for_actor(
    info: &VoiceConnectionInfo,
    sequence_ack: Option<u16>,
    cancellation: &TokioCancellationToken,
) -> Result<Option<VoiceWebSocket>, VoiceError> {
    match resume_gateway_cancellable(info, sequence_ack, cancellation).await {
        Err(VoiceError::Cancelled) if cancellation.is_cancelled() => Ok(None),
        Ok(websocket) => Ok(Some(websocket)),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remedia_core::CancellationToken;

    #[test]
    fn mock_packet_barriers_wait_for_target_without_blocking_other_channels() {
        for finish in [false, true] {
            let voice = TestVoiceConnection::new(42);
            let (ack, result) = mpsc::sync_channel(1);
            let control = if finish {
                Control::Finish { target_id: 2, ack }
            } else {
                Control::Pause { target_id: 2, ack }
            };
            voice.connection.inner.control_tx.send(control).unwrap();

            // A later control acknowledgement proves the actor observed the
            // barrier before any packets, and remained able to receive controls.
            let (ack, resumed) = mpsc::sync_channel(1);
            voice
                .connection
                .inner
                .control_tx
                .send(Control::Resume { ack })
                .unwrap();
            resumed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
            assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));

            for id in 1..=2 {
                voice
                    .connection
                    .inner
                    .packet_tx
                    .send(QueuedPacket {
                        generation: 0,
                        id,
                        packet: EncodedOpusPacket {
                            bytes: SILENCE_PACKET.to_vec(),
                            samples_per_channel: 960,
                            media_samples_per_channel: 960,
                            start_sample: (id - 1) * 960,
                        },
                    })
                    .unwrap();
                voice.wait_for_packets(id as usize);
                if id == 1 {
                    assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
                }
            }
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn endpoint_is_secure_and_pins_voice_gateway_v8() {
        let endpoint = voice_endpoint("voice.example.test:443").unwrap();
        assert_eq!(endpoint.scheme(), "wss");
        assert!(
            endpoint
                .query_pairs()
                .any(|(key, value)| key == "v" && value == "8")
        );
        assert!(
            endpoint
                .query_pairs()
                .any(|(key, value)| key == "encoding" && value == "json")
        );
        assert!(voice_endpoint("ws://voice.example.test").is_err());
    }

    #[test]
    fn close_codes_resume_only_when_discord_allows_it() {
        assert_eq!(voice_close_policy(4015), VoiceClosePolicy::Resume);
        assert_eq!(voice_close_policy(1006), VoiceClosePolicy::Resume);
        assert_eq!(voice_close_policy(4006), VoiceClosePolicy::FreshIdentify);
        assert_eq!(voice_close_policy(4009), VoiceClosePolicy::FreshIdentify);
        assert_eq!(voice_close_policy(4014), VoiceClosePolicy::WaitForGateway);
        assert_eq!(voice_close_policy(4021), VoiceClosePolicy::WaitForGateway);
        assert_eq!(voice_close_policy(4022), VoiceClosePolicy::WaitForGateway);
    }

    #[test]
    fn connection_debug_redacts_voice_credentials() {
        let info = VoiceConnectionInfo {
            guild_id: 1,
            channel_id: 2,
            user_id: 3,
            session_id: "private-session-value".into(),
            token: "private-token-value".into(),
            endpoint: "voice.example.test/?token=private-endpoint-value".into(),
        };
        let debug = format!("{info:?}");
        assert!(!debug.contains("private-session-value"));
        assert!(!debug.contains("private-token-value"));
        assert!(!debug.contains("private-endpoint-value"));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn parses_ready_fields_and_rejects_invalid_secret_key_sizes() {
        let ready = parse_ready(&json!({
            "ssrc": 0x1020,
            "ip": "192.0.2.5",
            "port": 4433,
            "modes": ["aead_xchacha20_poly1305_rtpsize", "xsalsa20_poly1305_lite"]
        }))
        .unwrap();
        assert_eq!(ready.ssrc, 0x1020);
        assert_eq!(ready.ip, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5)));
        assert_eq!(ready.port, 4433);
        assert_eq!(ready.modes.len(), 2);
        assert!(matches!(
            parse_secret_key(&json!({ "secret_key": [0, 1, 2] })),
            Err(VoiceError::InvalidSessionKey)
        ));
    }

    #[tokio::test]
    async fn udp_discovery_reports_external_address_and_keeps_socket_open() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut request = [0; 74];
            let (_, client) = server.recv_from(&mut request).await.unwrap();
            let ssrc = u32::from_be_bytes(request[4..8].try_into().unwrap());
            let mut response = [0; 74];
            response[..2].copy_from_slice(&2_u16.to_be_bytes());
            response[2..4].copy_from_slice(&70_u16.to_be_bytes());
            response[4..8].copy_from_slice(&ssrc.to_be_bytes());
            response[8..20].copy_from_slice(b"203.0.113.8\0");
            response[72..74].copy_from_slice(&55123_u16.to_be_bytes());
            server.send_to(&response, client).await.unwrap();
        });

        let ready = VoiceReady {
            ssrc: 0x1020_3040,
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: server_addr.port(),
            modes: vec!["aead_xchacha20_poly1305_rtpsize".into()],
        };
        let (socket, external) = establish_udp(&ready).await.unwrap();
        assert_eq!(external, ("203.0.113.8".parse().unwrap(), 55123));
        assert_ne!(socket.local_addr().unwrap().port(), 0);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn voice_handshake_uses_dave_v8_and_negotiates_udp_with_mock_endpoints() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let udp_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_address = udp_server.local_addr().unwrap();
        let discovery_task = tokio::spawn(async move {
            let mut request = [0; 74];
            let (_, client) = udp_server.recv_from(&mut request).await.unwrap();
            let ssrc = u32::from_be_bytes(request[4..8].try_into().unwrap());
            let mut response = [0; 74];
            response[..2].copy_from_slice(&2_u16.to_be_bytes());
            response[2..4].copy_from_slice(&70_u16.to_be_bytes());
            response[4..8].copy_from_slice(&ssrc.to_be_bytes());
            response[8..20].copy_from_slice(b"203.0.113.8\0");
            response[72..74].copy_from_slice(&55123_u16.to_be_bytes());
            udp_server.send_to(&response, client).await.unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = listener.local_addr().unwrap();
        let gateway_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let identify = websocket.next().await.unwrap().unwrap();
            let identify: Value =
                serde_json::from_str(identify.into_text().unwrap().as_str()).unwrap();
            assert_eq!(identify["op"], 0);
            assert_eq!(
                identify["d"]["max_dave_protocol_version"],
                DAVE_PROTOCOL_VERSION
            );

            websocket
                .send(Message::Text(
                    json!({ "op": 8, "d": { "heartbeat_interval": 1_000 } })
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    json!({
                        "op": 2,
                        "d": {
                            "ssrc": 0x1020_3040_u32,
                            "ip": "127.0.0.1",
                            "port": udp_address.port(),
                            "modes": ["aead_xchacha20_poly1305_rtpsize"],
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            let select = websocket.next().await.unwrap().unwrap();
            let select: Value = serde_json::from_str(select.into_text().unwrap().as_str()).unwrap();
            assert_eq!(select["op"], 1);
            assert_eq!(
                select["d"]["data"]["mode"],
                "aead_xchacha20_poly1305_rtpsize"
            );

            websocket
                .send(Message::Text(
                    json!({ "op": 11, "d": { "user_ids": ["42"] }, "seq": 8 })
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    json!({
                        "op": 4,
                        "d": {
                            "mode": "aead_xchacha20_poly1305_rtpsize",
                            "secret_key": vec![0_u8; 32],
                            "dave_protocol_version": DAVE_PROTOCOL_VERSION,
                        },
                        "seq": 9,
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let endpoint = Url::parse(&format!("ws://{ws_address}/")).unwrap();
        let info = VoiceConnectionInfo {
            guild_id: 10,
            channel_id: 20,
            user_id: 30,
            session_id: "test-session".into(),
            token: "test-token".into(),
            endpoint: ws_address.to_string(),
        };
        let (websocket, interval, ready, early, session, udp, sequence_ack) =
            establish_voice_session(&endpoint, &info, 1).await.unwrap();
        assert_eq!(interval, Duration::from_secs(1));
        assert_eq!(ready.ssrc, 0x1020_3040);
        assert_eq!(early.len(), 1);
        assert_eq!(sequence_ack, Some(9));
        assert_eq!(session["dave_protocol_version"], DAVE_PROTOCOL_VERSION);
        assert!(udp.local_addr().unwrap().port() > 0);
        drop(websocket);
        gateway_task.await.unwrap();
        discovery_task.await.unwrap();
    }

    #[test]
    fn packet_pacing_keeps_deadlines_and_rebases_late_frames_without_catchup() {
        let start = Instant::now();
        let frame = Duration::from_millis(20);
        assert_eq!(
            packet_deadline(start, start + Duration::from_millis(5), frame),
            Some(start + frame)
        );
        assert_eq!(
            packet_deadline(start, start + Duration::from_millis(21), frame),
            None
        );
        assert_eq!(
            paced_deadline(start, start + Duration::from_millis(35), frame),
            start + Duration::from_millis(55)
        );
    }

    #[test]
    fn output_cancel_fallback_discards_stale_packets_and_is_idempotent() {
        let mut queued = VecDeque::from([QueuedPacket {
            generation: 3,
            id: 4,
            packet: EncodedOpusPacket {
                bytes: vec![1],
                samples_per_channel: 960,
                media_samples_per_channel: 960,
                start_sample: 0,
            },
        }]);
        let mut processed_id = 2;
        let mut barrier = None;
        let mut paused = true;
        let mut playback_active = true;
        let mut next_send_at = None;

        begin_output_cancel(
            &mut queued,
            &mut processed_id,
            &mut barrier,
            &mut paused,
            &mut playback_active,
            true,
            &mut next_send_at,
        );
        assert!(queued.is_empty());
        assert_eq!(processed_id, 4);
        assert!(!paused);
        assert!(!playback_active);
        assert!(next_send_at.is_some());
        let silence_packets = barrier.as_ref().unwrap().silence_packets;
        assert_eq!(silence_packets, 5);

        begin_output_cancel(
            &mut queued,
            &mut processed_id,
            &mut barrier,
            &mut paused,
            &mut playback_active,
            true,
            &mut next_send_at,
        );
        assert_eq!(barrier.as_ref().unwrap().silence_packets, silence_packets);
    }

    #[test]
    fn new_output_packet_supersedes_an_old_cancel_barrier() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let mut barrier = Some(PendingBarrier {
            target_id: 8,
            silence_packets: 4,
            kind: BarrierKind::Cancel,
            ack: None,
        });
        let mut paused = false;
        let mut playback_active = false;
        let mut next_send_at = Some(deadline);

        assert!(supersede_cancel_barrier(
            &mut barrier,
            &mut paused,
            &mut playback_active,
            &mut next_send_at,
        ));
        assert!(barrier.is_none());
        assert!(!paused);
        assert!(playback_active);
        assert_eq!(next_send_at, Some(deadline));
    }

    #[test]
    fn coalesced_generation_update_does_not_reapply_an_old_cancel() {
        let cancelled_generation = AtomicU64::new(2);
        let mut active_generation = 1;

        // Cancellation published generation 2, then the replacement sender
        // published generation 3 before the actor observed the watch change.
        assert!(!synchronize_output_generation(
            3,
            &mut active_generation,
            &cancelled_generation,
        ));

        assert_eq!(active_generation, 3);
        assert!(!cancellation_matches_generation(
            1,
            3,
            &cancelled_generation
        ));
    }

    #[test]
    fn current_cancel_generation_starts_exactly_one_cancel_barrier() {
        let cancelled_generation = AtomicU64::new(2);
        let mut active_generation = 1;
        let mut queued = VecDeque::from([QueuedPacket {
            generation: 1,
            id: 7,
            packet: EncodedOpusPacket {
                bytes: vec![1],
                samples_per_channel: 960,
                media_samples_per_channel: 960,
                start_sample: 0,
            },
        }]);
        let mut processed_id = 3;
        let mut barrier = None;
        let mut paused = false;
        let mut playback_active = true;
        let mut next_send_at = None;

        assert!(cancellation_matches_generation(1, 2, &cancelled_generation));
        assert!(synchronize_output_generation(
            2,
            &mut active_generation,
            &cancelled_generation,
        ));
        begin_output_cancel(
            &mut queued,
            &mut processed_id,
            &mut barrier,
            &mut paused,
            &mut playback_active,
            true,
            &mut next_send_at,
        );

        assert_eq!(active_generation, 2);
        assert!(queued.is_empty());
        assert_eq!(processed_id, 7);
        assert!(!playback_active);
        assert!(matches!(
            barrier.as_ref().map(|pending| &pending.kind),
            Some(BarrierKind::Cancel)
        ));
    }

    #[test]
    fn concurrent_sender_replacement_and_drop_publish_the_latest_generation() {
        let (packet_tx, _packet_rx) = flume::bounded(QUEUE_CAPACITY);
        let (control_tx, _control_rx) = flume::bounded(16);
        let (generation_tx, generation_rx) = watch::channel(0);
        let (state, _) = watch::channel(ConnectionState::Connected);
        let connection = DiscordVoiceConnection {
            inner: Arc::new(ConnectionInner {
                voice_connection_id: 4,
                guild_id: 10,
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
                cancellation: TokioCancellationToken::new(),
                worker: Mutex::new(None),
            }),
        };
        let gate = Arc::new(std::sync::Barrier::new(9));
        let workers = (0..8)
            .map(|_| {
                let connection = connection.clone();
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    for _ in 0..100 {
                        drop(connection.opus_sender());
                    }
                })
            })
            .collect::<Vec<_>>();

        gate.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(
            *generation_rx.borrow(),
            connection.inner.generation.load(Ordering::Acquire)
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_resume_attempt() {
        let info = VoiceConnectionInfo {
            guild_id: 1,
            channel_id: 2,
            user_id: 3,
            session_id: "session".into(),
            token: "token".into(),
            endpoint: "voice.example.test".into(),
        };
        let cancellation = TokioCancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            resume_gateway_cancellable(&info, None, &cancellation).await,
            Err(VoiceError::Cancelled)
        ));
    }

    #[test]
    fn heartbeat_ack_must_match_and_timeout_uses_elapsed_monotonic_time() {
        let now = Instant::now();
        let interval = Duration::from_secs(5);
        assert!(heartbeat_ack_matches(Some(7), Some(7)));
        assert!(!heartbeat_ack_matches(Some(7), Some(8)));
        assert!(!heartbeat_ack_matches(None, None));
        assert!(!heartbeat_expired(
            now,
            now + Duration::from_secs(10),
            interval
        ));
        assert!(heartbeat_expired(
            now,
            now + Duration::from_millis(10_001),
            interval
        ));
    }

    #[test]
    fn opus_sender_backpressures_at_its_bounded_queue_and_cancellation_unblocks() {
        let (packet_tx, packet_rx) = flume::bounded(QUEUE_CAPACITY);
        let (control_tx, _control_rx) = flume::bounded(16);
        let (generation_tx, _generation_rx) = watch::channel(0);
        let (state, _) = watch::channel(ConnectionState::Connected);
        let inner = Arc::new(ConnectionInner {
            voice_connection_id: 1,
            guild_id: 10,
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
            cancellation: TokioCancellationToken::new(),
            worker: Mutex::new(None),
        });
        let sender = Arc::new(DiscordOpusSender {
            inner,
            generation: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        sender
            .inner
            .state
            .send_replace(ConnectionState::Reconnecting);
        let packet = || EncodedOpusPacket {
            bytes: vec![1],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        };
        let token = CancellationToken::new();
        for _ in 0..QUEUE_CAPACITY {
            sender.send_packet(packet(), &token).unwrap();
        }

        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let blocked_sender = Arc::clone(&sender);
        let blocked_token = token.clone();
        let worker = std::thread::spawn(move || {
            let result = blocked_sender.send_packet(packet(), &blocked_token);
            let _ = result_tx.send(result);
        });
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(60)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        token.cancel();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(OutputError::Cancelled)
        ));
        worker.join().unwrap();
        assert_eq!(packet_rx.len(), QUEUE_CAPACITY);
        sender.cancel();
        assert_eq!(sender.inner.cancelled_generation.load(Ordering::Acquire), 1);
    }
}
