use super::*;

pub(super) struct VoiceActorSetup {
    pub(super) voice_connection_id: u64,
    pub(super) metrics: remedia_observability::RemediaMetrics,
    pub(super) active_guard: ActiveVoiceConnection,
    pub(super) websocket: VoiceWebSocket,
    pub(super) udp: UdpSocket,
    pub(super) udp_server: SocketAddr,
    pub(super) info: VoiceConnectionInfo,
    pub(super) ssrc: u32,
    pub(super) heartbeat_interval: Duration,
    pub(super) pending_gateway_messages: Vec<GatewayMessage>,
    pub(super) sequence_ack: Option<u16>,
    pub(super) dave: Arc<Mutex<DaveProtocol>>,
    pub(super) rtp: RtpSender,
    pub(super) packet_rx: Receiver<QueuedPacket>,
    pub(super) control_rx: Receiver<Control>,
    pub(super) generation_rx: watch::Receiver<u64>,
    pub(super) cancelled_generation: Arc<AtomicU64>,
    pub(super) cancellation: TokioCancellationToken,
    pub(super) state: watch::Sender<ConnectionState>,
}

pub(super) async fn voice_actor(setup: VoiceActorSetup) -> Result<(), VoiceError> {
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
    let mut scheduler = PacketScheduler::new(*generation_rx.borrow());
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
        let deadline = scheduler.next_send_at;
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
            packet = packet_rx.recv_async(), if scheduler.queued.len() < QUEUE_CAPACITY && dave_ready => Event::Packet(packet),
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
                    &mut scheduler.active_generation,
                    &cancelled_generation,
                ) {
                    begin_output_cancel(
                        &mut scheduler.queued,
                        &mut scheduler.processed_id,
                        &mut scheduler.barrier,
                        &mut scheduler.paused,
                        &mut scheduler.playback_active,
                        scheduler.speaking,
                        &mut scheduler.next_send_at,
                    );
                }
                if packet.generation == scheduler.active_generation {
                    if supersede_cancel_barrier(
                        &mut scheduler.barrier,
                        &mut scheduler.paused,
                        &mut scheduler.playback_active,
                        &mut scheduler.next_send_at,
                    ) {
                        tracing::debug!(
                            name: "discord.rtp_sender.cancel_superseded",
                            target: "remedia_discord::connection",
                            voice_connection_id,
                            generation = scheduler.active_generation,
                            "new playback superseded the previous sender cancellation barrier"
                        );
                    }
                    scheduler.playback_active = true;
                    scheduler.queued.push_back(packet);
                    if scheduler.next_send_at.is_none() && !scheduler.paused {
                        scheduler.next_send_at = Some(Instant::now());
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
                    scheduler.processed_id = scheduler.processed_id.max(packet.id);
                }
            }
            Event::Packet(Err(_)) => return Err(VoiceError::Disconnected),
            Event::Control(Ok(Control::Flush { generation, ack })) => {
                scheduler.active_generation = generation;
                metrics.rtp_packet_dropped_by(
                    u64::try_from(scheduler.queued.len()).unwrap_or(u64::MAX),
                );
                while let Some(packet) = scheduler.queued.pop_front() {
                    scheduler.processed_id = scheduler.processed_id.max(packet.id);
                }
                if !scheduler.paused && scheduler.playback_active {
                    scheduler.next_send_at = Some(Instant::now());
                }
                let _ = ack.send(Ok(()));
            }
            Event::Control(Ok(Control::Pause { target_id, ack })) => {
                let silence_packets = if scheduler.speaking { 5 } else { 0 };
                scheduler.barrier = Some(PendingBarrier {
                    target_id,
                    silence_packets,
                    kind: BarrierKind::Pause,
                    ack: Some(ack),
                });
                if scheduler.next_send_at.is_none() {
                    scheduler.next_send_at = Some(Instant::now());
                }
            }
            Event::Control(Ok(Control::Resume { ack })) => {
                if !scheduler.paused {
                    let _ = ack.send(Err(VoiceError::Protocol(
                        "output resume was requested while it was not paused".into(),
                    )));
                } else {
                    scheduler.paused = false;
                    scheduler.playback_active = true;
                    send_speaking(&mut websocket, ssrc, true, voice_connection_id).await?;
                    scheduler.speaking = true;
                    scheduler.next_send_at = None;
                    let _ = ack.send(Ok(()));
                }
            }
            Event::Control(Ok(Control::Finish { target_id, ack })) => {
                let silence_packets = if scheduler.speaking { 5 } else { 0 };
                scheduler.barrier = Some(PendingBarrier {
                    target_id,
                    silence_packets,
                    kind: BarrierKind::Finish,
                    ack: Some(ack),
                });
                if scheduler.next_send_at.is_none() {
                    scheduler.next_send_at = Some(Instant::now());
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
                    &mut scheduler.active_generation,
                    &cancelled_generation,
                ) {
                    begin_output_cancel(
                        &mut scheduler.queued,
                        &mut scheduler.processed_id,
                        &mut scheduler.barrier,
                        &mut scheduler.paused,
                        &mut scheduler.playback_active,
                        scheduler.speaking,
                        &mut scheduler.next_send_at,
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
                scheduler
                    .send_due(
                        deadline,
                        PacketTransport {
                            udp: &udp,
                            udp_server,
                            rtp: &mut rtp,
                            dave: &dave,
                            packet_observability: &packet_observability,
                            websocket: &mut websocket,
                            ssrc,
                            voice_connection_id,
                            metrics: &metrics,
                        },
                    )
                    .await?;
            }
            Event::Generation(Ok(())) => {
                let generation = *generation_rx.borrow_and_update();
                if synchronize_output_generation(
                    generation,
                    &mut scheduler.active_generation,
                    &cancelled_generation,
                ) {
                    begin_output_cancel(
                        &mut scheduler.queued,
                        &mut scheduler.processed_id,
                        &mut scheduler.barrier,
                        &mut scheduler.paused,
                        &mut scheduler.playback_active,
                        scheduler.speaking,
                        &mut scheduler.next_send_at,
                    );
                }
            }
            Event::Generation(Err(_)) => return Ok(()),
        }
    }
}

mod scheduler;
use scheduler::{PacketScheduler, PacketTransport};
