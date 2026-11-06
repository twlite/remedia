use super::*;

pub(super) fn voice_endpoint(endpoint: &str) -> Result<Url, VoiceError> {
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

pub(super) struct VoiceReady {
    pub(super) ssrc: u32,
    pub(super) ip: IpAddr,
    pub(super) port: u16,
    pub(super) modes: Vec<String>,
}

pub(super) async fn establish_voice_session(
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

pub(super) fn parse_ready(data: &Value) -> Result<VoiceReady, VoiceError> {
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

pub(super) fn parse_secret_key(data: &Value) -> Result<Vec<u8>, VoiceError> {
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

pub(super) async fn establish_udp(
    ready: &VoiceReady,
) -> Result<(UdpSocket, (IpAddr, u16)), VoiceError> {
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

pub(super) async fn select_protocol(
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

pub(super) async fn send_json(
    websocket: &mut VoiceWebSocket,
    value: &Value,
) -> Result<(), VoiceError> {
    send_ws_message(websocket, Message::Text(value.to_string().into())).await
}

pub(super) async fn send_ws_message(
    websocket: &mut VoiceWebSocket,
    message: Message,
) -> Result<(), VoiceError> {
    time::timeout(Duration::from_secs(5), websocket.send(message))
        .await
        .map_err(|_| VoiceError::Timeout("sending on the voice WebSocket"))??;
    Ok(())
}

pub(super) fn is_dave_message(message: &GatewayMessage) -> bool {
    match message {
        GatewayMessage::Json { opcode, .. } => matches!(*opcode, 11 | 13 | 21 | 22 | 24 | 31),
        GatewayMessage::Binary { opcode, .. } => matches!(*opcode, 25 | 27 | 29 | 30),
    }
}

pub(super) fn gateway_opcode(message: &GatewayMessage) -> u64 {
    match message {
        GatewayMessage::Json { opcode, .. } => *opcode,
        GatewayMessage::Binary { opcode, .. } => u64::from(*opcode),
    }
}
