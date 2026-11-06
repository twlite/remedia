use super::*;

/// A playback-scoped synchronous sink that feeds the connection's bounded
/// real-time packet queue. It implements `remedia_core::OpusPacketSink`.
pub struct DiscordOpusSender {
    pub(super) inner: Arc<ConnectionInner>,
    pub(super) generation: AtomicU64,
    pub(super) cancelled: AtomicBool,
    pub(super) finished: AtomicBool,
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

    pub(super) fn ensure_current(
        &self,
        cancellation: &CoreCancellationToken,
    ) -> Result<u64, VoiceError> {
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

    pub(super) fn failure_message(&self) -> String {
        self.inner
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or_else(|| "voice transport failed".into(), ToString::to_string)
    }

    pub(super) fn wait_for_control(
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

    pub(super) fn request_barrier(
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
