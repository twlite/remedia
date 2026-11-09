//! RTP deadlines, packet pacing, and output barriers for the voice actor.

use super::*;

pub(super) struct PacketScheduler {
    pub(super) active_generation: u64,
    pub(super) queued: VecDeque<QueuedPacket>,
    pub(super) processed_id: u64,
    pub(super) barrier: Option<PendingBarrier>,
    pub(super) paused: bool,
    pub(super) playback_active: bool,
    pub(super) speaking: bool,
    pub(super) next_send_at: Option<Instant>,
}

pub(super) struct PacketTransport<'a> {
    pub(super) udp: &'a UdpSocket,
    pub(super) udp_server: SocketAddr,
    pub(super) rtp: &'a mut RtpSender,
    pub(super) dave: &'a Arc<Mutex<DaveProtocol>>,
    pub(super) packet_observability: &'a RtpSendObservability,
    pub(super) websocket: &'a mut VoiceWebSocket,
    pub(super) ssrc: u32,
    pub(super) voice_connection_id: u64,
    pub(super) metrics: &'a remedia_observability::RemediaMetrics,
}

impl PacketScheduler {
    pub(super) fn new(active_generation: u64) -> Self {
        Self {
            active_generation,
            queued: VecDeque::with_capacity(QUEUE_CAPACITY),
            processed_id: 0,
            barrier: None,
            paused: false,
            playback_active: false,
            speaking: false,
            next_send_at: None,
        }
    }

    pub(super) async fn send_due(
        &mut self,
        deadline: Option<Instant>,
        transport: PacketTransport<'_>,
    ) -> Result<(), VoiceError> {
        let PacketTransport {
            udp,
            udp_server,
            rtp,
            dave,
            packet_observability,
            websocket,
            ssrc,
            voice_connection_id,
            metrics,
        } = transport;
        let now = Instant::now();
        let scheduled_deadline = deadline.unwrap_or(now);
        if let Some(pending) = self.barrier.as_mut()
            && self.processed_id >= pending.target_id
        {
            if pending.silence_packets > 0 {
                send_transport_packet(
                    udp,
                    udp_server,
                    rtp,
                    dave,
                    packet_observability,
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
                self.next_send_at = Some(paced_deadline(
                    scheduled_deadline,
                    now,
                    Duration::from_millis(20),
                ));
                return Ok(());
            }

            let action = self.barrier.take().expect("barrier exists");
            send_speaking(websocket, ssrc, false, voice_connection_id).await?;
            self.speaking = false;
            match action.kind {
                BarrierKind::Pause => {
                    self.paused = true;
                    self.playback_active = false;
                    self.next_send_at = None;
                }
                BarrierKind::Finish | BarrierKind::Cancel => {
                    self.paused = false;
                    self.playback_active = false;
                    self.next_send_at = None;
                }
            }
            if let Some(ack) = action.ack {
                let _ = ack.send(Ok(()));
            }
            return Ok(());
        }

        if self.paused || !self.playback_active {
            self.next_send_at = None;
            return Ok(());
        }
        if !lock(dave).is_ready() {
            self.next_send_at = Some(now + Duration::from_millis(20));
            return Ok(());
        }
        if !self.speaking {
            send_speaking(websocket, ssrc, true, voice_connection_id).await?;
            self.speaking = true;
        }

        while self
            .queued
            .front()
            .is_some_and(|packet| packet.generation != self.active_generation)
        {
            if let Some(packet) = self.queued.pop_front() {
                metrics.rtp_packet_dropped();
                self.processed_id = self.processed_id.max(packet.id);
                rtp.skip_packet_duration(packet.packet.samples_per_channel);
            }
        }

        let packet = self.queued.pop_front();
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
                    self.processed_id = self.processed_id.max(packet.id);
                    self.next_send_at = Some(now + duration);
                }
                Some(next_deadline) => {
                    send_transport_packet(
                        udp,
                        udp_server,
                        rtp,
                        dave,
                        packet_observability,
                        now.saturating_duration_since(scheduled_deadline),
                        &packet.packet,
                    )
                    .await?;
                    self.processed_id = self.processed_id.max(packet.id);
                    self.next_send_at = Some(next_deadline);
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
                udp,
                udp_server,
                rtp,
                dave,
                packet_observability,
                now.saturating_duration_since(scheduled_deadline),
                &silence,
            )
            .await?;
            self.next_send_at = Some(paced_deadline(
                scheduled_deadline,
                now,
                Duration::from_millis(20),
            ));
        }
        Ok(())
    }
}
