use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VoiceClosePolicy {
    /// Resume is valid for an interrupted session, including Discord's
    /// recoverable voice-server-crash close code 4015.
    Resume,
    /// The voice session expired; the Gateway must provide fresh credentials.
    FreshIdentify,
    /// Discord says not to reconnect until a fresh Gateway voice update arrives.
    WaitForGateway,
}

pub(super) fn voice_close_policy(code: u16) -> VoiceClosePolicy {
    match code {
        4006 | 4009 => VoiceClosePolicy::FreshIdentify,
        4014 | 4021 | 4022 => VoiceClosePolicy::WaitForGateway,
        _ => VoiceClosePolicy::Resume,
    }
}

pub(super) async fn process_gateway_message(
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

pub(super) fn begin_output_cancel(
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

pub(super) fn cancellation_matches_generation(
    cancelled_output_generation: u64,
    published_generation: u64,
    cancelled_generation: &AtomicU64,
) -> bool {
    published_generation == cancelled_output_generation.wrapping_add(1)
        && cancelled_generation.load(Ordering::Acquire) == published_generation
}

pub(super) fn synchronize_output_generation(
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
pub(super) fn supersede_cancel_barrier(
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

pub(super) async fn send_speaking(
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

pub(super) async fn send_transport_packet(
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
pub(super) fn packet_deadline(
    scheduled: Instant,
    now: Instant,
    duration: Duration,
) -> Option<Instant> {
    let next = scheduled + duration;
    if now > next { None } else { Some(next) }
}

pub(super) fn paced_deadline(scheduled: Instant, now: Instant, duration: Duration) -> Instant {
    (scheduled + duration).max(now + duration)
}

pub(super) fn heartbeat_ack_matches(pending: Option<u64>, received: Option<u64>) -> bool {
    pending.is_some() && pending == received
}

pub(super) fn heartbeat_expired(last_ack: Instant, now: Instant, interval: Duration) -> bool {
    now.duration_since(last_ack) > interval.saturating_mul(2)
}
