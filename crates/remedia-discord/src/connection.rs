use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use flume::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use rand::random;
use remedia_core::{
    CancellationToken as CoreCancellationToken, EncodedOpusPacket, OpusPacketSink, OutputError,
};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken as TokioCancellationToken;
use url::Url;

use crate::crypto::EncryptionMode;
use crate::dave::{DaveProtocol, OutgoingGatewayMessage};
use crate::error::VoiceError;
use crate::protocol::{
    GatewayMessage, ip_discovery_request, json_message, parse_ip_discovery_response, parse_u16,
};
use crate::rtp::{RtpSender, validate_opus_packet};

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
            .field("endpoint", &self.endpoint)
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
    state: watch::Sender<ConnectionState>,
    failure: Arc<Mutex<Option<Arc<VoiceError>>>>,
    packet_tx: Sender<QueuedPacket>,
    control_tx: Sender<Control>,
    generation: AtomicU64,
    generation_tx: watch::Sender<u64>,
    cancelled_generation: Arc<AtomicU64>,
    accepted_packet_id: AtomicU64,
    next_packet_id: AtomicU64,
    cancellation: TokioCancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
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

impl DiscordVoiceConnection {
    /// Connect to Discord voice using session information from an external
    /// Gateway client. This performs the voice WebSocket handshake, UDP IP
    /// discovery, encryption selection, and DAVE setup.
    pub async fn connect(info: VoiceConnectionInfo) -> Result<Self, VoiceError> {
        let endpoint = voice_endpoint(&info.endpoint)?;
        let (mut websocket, hello_interval, ready, early_messages, session_data, udp, sequence_ack) =
            establish_voice_session(&endpoint, &info).await?;

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

        let mut dave = DaveProtocol::new(dave_version, info.user_id, info.channel_id)?;
        let initial_key_package = dave.create_key_package()?;
        send_ws_message(&mut websocket, Message::Binary(initial_key_package.into())).await?;
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

        let worker = tokio::spawn(async move {
            let result = voice_actor(VoiceActorSetup {
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
                *lock(&actor_failure) = Some(Arc::new(error));
                actor_state.send_replace(ConnectionState::Failed);
            } else if actor_state.borrow().eq(&ConnectionState::Connected) {
                actor_state.send_replace(ConnectionState::Disconnected);
            }
        });

        let connection = Self {
            inner: Arc::new(ConnectionInner {
                state,
                failure,
                packet_tx,
                control_tx,
                generation: AtomicU64::new(0),
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
            Ok(Ok(())) => Ok(connection),
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
        let generation = self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner.generation_tx.send_replace(generation);
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
        if self.state() == ConnectionState::Disconnected {
            self.join_worker().await;
            return Ok(());
        }
        if self.state() == ConnectionState::Failed {
            self.inner.cancellation.cancel();
            self.join_worker().await;
            return Ok(());
        }

        self.inner
            .state
            .send_replace(ConnectionState::Disconnecting);
        self.inner.cancellation.cancel();
        self.join_worker().await;
        Ok(())
    }

    async fn join_worker(&self) {
        let worker = lock(&self.inner.worker).take();
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                *lock(&self.inner.failure) = Some(Arc::new(VoiceError::Worker(error.to_string())));
                self.inner.state.send_replace(ConnectionState::Failed);
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

    fn flush(
        &self,
        _position: Duration,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
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
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
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
    }
}

impl Drop for DiscordOpusSender {
    fn drop(&mut self) {
        self.cancel();
    }
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
    let (mut websocket, _) = time::timeout(CONNECT_TIMEOUT, connect_async(endpoint.as_str()))
        .await
        .map_err(|_| VoiceError::Timeout("connecting to the voice gateway"))??;
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
                        ready = Some(parse_ready(data)?);
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

    let (udp, external) = establish_udp(&ready).await?;
    let mode = EncryptionMode::choose(&ready.modes)?;
    select_protocol(&mut websocket, external, mode).await?;

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

struct VoiceActorSetup {
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
        process_gateway_message(&mut websocket, &dave, message).await?;
        if lock(&dave).is_ready() {
            state.send_replace(ConnectionState::Connected);
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

        match event {
            Event::Cancel => {
                let _ = send_speaking(&mut websocket, ssrc, false).await;
                state.send_replace(ConnectionState::Disconnected);
                return Ok(());
            }
            Event::Socket(None) | Event::Socket(Some(Ok(Message::Close(_)))) => {
                state.send_replace(ConnectionState::Reconnecting);
                let Some(resumed) = resume_for_actor(&info, sequence_ack, &cancellation).await?
                else {
                    state.send_replace(ConnectionState::Disconnected);
                    return Ok(());
                };
                websocket = resumed;
                state.send_replace(if lock(&dave).is_ready() {
                    ConnectionState::Connected
                } else {
                    ConnectionState::NegotiatingDave
                });
                last_heartbeat_ack = Instant::now();
                pending_heartbeat_nonce = None;
            }
            Event::Socket(Some(Err(_))) => {
                state.send_replace(ConnectionState::Reconnecting);
                let Some(resumed) = resume_for_actor(&info, sequence_ack, &cancellation).await?
                else {
                    state.send_replace(ConnectionState::Disconnected);
                    return Ok(());
                };
                websocket = resumed;
                state.send_replace(if lock(&dave).is_ready() {
                    ConnectionState::Connected
                } else {
                    ConnectionState::NegotiatingDave
                });
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
                        process_gateway_message(&mut websocket, &dave, message).await?;
                        if lock(&dave).is_ready() {
                            state.send_replace(ConnectionState::Connected);
                        }
                    }
                }
            }
            Event::Packet(Ok(packet)) => {
                if packet.generation == active_generation {
                    playback_active = true;
                    queued.push_back(packet);
                    if next_send_at.is_none() && !paused {
                        next_send_at = Some(Instant::now());
                    }
                } else {
                    processed_id = processed_id.max(packet.id);
                }
            }
            Event::Packet(Err(_)) => return Err(VoiceError::Disconnected),
            Event::Control(Ok(Control::Flush { generation, ack })) => {
                active_generation = generation;
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
                    send_speaking(&mut websocket, ssrc, true).await?;
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
                if generation == active_generation
                    || generation.wrapping_add(1) == active_generation
                {
                    active_generation = generation.wrapping_add(1);
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
                    state.send_replace(ConnectionState::Reconnecting);
                    let Some(resumed) =
                        resume_for_actor(&info, sequence_ack, &cancellation).await?
                    else {
                        state.send_replace(ConnectionState::Disconnected);
                        return Ok(());
                    };
                    websocket = resumed;
                    state.send_replace(if lock(&dave).is_ready() {
                        ConnectionState::Connected
                    } else {
                        ConnectionState::NegotiatingDave
                    });
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
                    send_speaking(&mut websocket, ssrc, false).await?;
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
                    send_speaking(&mut websocket, ssrc, true).await?;
                    speaking = true;
                }

                while queued
                    .front()
                    .is_some_and(|packet| packet.generation != active_generation)
                {
                    if let Some(packet) = queued.pop_front() {
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
                                &packet.packet,
                            )
                            .await?;
                            processed_id = processed_id.max(packet.id);
                            next_send_at = Some(next_deadline);
                        }
                    }
                } else {
                    let silence = EncodedOpusPacket {
                        bytes: SILENCE_PACKET.to_vec(),
                        samples_per_channel: 960,
                        media_samples_per_channel: 960,
                        start_sample: 0,
                    };
                    send_transport_packet(&udp, udp_server, &mut rtp, &dave, &silence).await?;
                    next_send_at = Some(paced_deadline(
                        scheduled_deadline,
                        now,
                        Duration::from_millis(20),
                    ));
                }
            }
            Event::Generation(Ok(())) => {
                active_generation = *generation_rx.borrow_and_update();
                queued.retain(|packet| {
                    let keep = packet.generation == active_generation;
                    if !keep {
                        processed_id = processed_id.max(packet.id);
                    }
                    keep
                });
                if cancelled_generation.load(Ordering::Acquire) == active_generation {
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

async fn process_gateway_message(
    websocket: &mut VoiceWebSocket,
    dave: &Arc<Mutex<DaveProtocol>>,
    message: GatewayMessage,
) -> Result<(), VoiceError> {
    let outgoing = lock(dave).handle(&message)?;
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

async fn send_speaking(
    websocket: &mut VoiceWebSocket,
    ssrc: u32,
    speaking: bool,
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
}

async fn send_transport_packet(
    udp: &UdpSocket,
    server: SocketAddr,
    rtp: &mut RtpSender,
    dave: &Arc<Mutex<DaveProtocol>>,
    packet: &EncodedOpusPacket,
) -> Result<(), VoiceError> {
    let bytes = rtp.encode_packet(packet, &mut lock(dave))?;
    let sent = time::timeout(Duration::from_secs(2), udp.send_to(&bytes, server))
        .await
        .map_err(|_| VoiceError::Timeout("sending an RTP packet"))??;
    if sent != bytes.len() {
        return Err(VoiceError::Io(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "UDP socket did not send a complete RTP packet",
        )));
    }
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
    fn connection_debug_redacts_voice_credentials() {
        let info = VoiceConnectionInfo {
            guild_id: 1,
            channel_id: 2,
            user_id: 3,
            session_id: "private-session-value".into(),
            token: "private-token-value".into(),
            endpoint: "voice.example.test".into(),
        };
        let debug = format!("{info:?}");
        assert!(!debug.contains("private-session-value"));
        assert!(!debug.contains("private-token-value"));
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
            establish_voice_session(&endpoint, &info).await.unwrap();
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
            state,
            failure: Arc::new(Mutex::new(None)),
            packet_tx,
            control_tx,
            generation: AtomicU64::new(0),
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
