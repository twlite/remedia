use super::*;

/// A playback-scoped synchronous sink that feeds the connection's bounded
/// real-time packet queue. It implements `remedia_core::OpusPacketSink`.
pub struct DiscordOpusSender {
    pub(super) inner: Arc<ConnectionInner>,
    pub(super) generation: AtomicU64,
    pub(super) cancelled: AtomicBool,
    pub(super) finished: AtomicBool,
    /// End sample of the last successfully dispatched audio packet; MAX means unknown.
    pub(super) played_end_sample: Arc<AtomicU64>,
    #[cfg(feature = "experimental-scheduler")]
    pub(super) pending_control: Mutex<Option<readiness::PendingControl>>,
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
        receiver: flume::Receiver<Result<(), VoiceError>>,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), VoiceError> {
        loop {
            if cancellation.is_cancelled() || self.inner.cancellation.is_cancelled() {
                return Err(VoiceError::Cancelled);
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(flume::RecvTimeoutError::Timeout) => {
                    // The actor owns the transport state. This short wait is
                    // bounded and interruptible rather than a polling loop.
                }
                Err(flume::RecvTimeoutError::Disconnected) => {
                    return Err(VoiceError::Disconnected);
                }
            }
        }
    }

    pub(super) fn request_barrier(
        &self,
        make_control: impl FnOnce(flume::Sender<Result<(), VoiceError>>) -> Control,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), VoiceError> {
        let (ack_tx, ack_rx) = flume::bounded(1);
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

impl DiscordOpusSender {
    /// Queue `packet`, returning it unchanged with the error when it was not accepted.
    ///
    /// A router can therefore retry the same allocation on a replacement transport instead of
    /// cloning every packet just in case the first attempt fails.
    pub(crate) fn try_send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), (OutputError, EncodedOpusPacket)> {
        if let Err(error) = validate_opus_packet(&packet) {
            return Err((OutputError::with_source(error), packet));
        }
        let generation = match self.ensure_current(cancellation) {
            Ok(generation) => generation,
            Err(error) => return Err((OutputError::with_source(error), packet)),
        };
        let id = self
            .inner
            .next_packet_id
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let mut queued = QueuedPacket {
            generation,
            id,
            packet,
            played_end_sample: Arc::clone(&self.played_end_sample),
        };
        loop {
            if cancellation.is_cancelled() || self.cancelled.load(Ordering::Acquire) {
                return Err((OutputError::Cancelled, queued.packet));
            }
            if self.inner.generation.load(Ordering::Acquire) != generation {
                return Err((
                    OutputError::with_source(VoiceError::Superseded),
                    queued.packet,
                ));
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
                Err(flume::SendTimeoutError::Disconnected(returned)) => {
                    return Err((
                        OutputError::with_source(VoiceError::Disconnected),
                        returned.packet,
                    ));
                }
            }
        }
    }
}

impl OpusPacketSink for DiscordOpusSender {
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_nonblocking(&self) -> bool {
        true
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_packet(
        &self,
        packet: &mut Option<EncodedOpusPacket>,
        cx: &mut std::task::Context<'_>,
        cancellation: &CoreCancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        self.poll_packet(packet, cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_control(
        &self,
        control: remedia_core::OutputControl,
        cx: &mut std::task::Context<'_>,
        cancellation: &CoreCancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        self.poll_control(control, cx, cancellation)
    }

    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), OutputError> {
        self.try_send_packet(packet, cancellation)
            .map_err(|(error, _)| error)
    }

    fn supports_passthrough_packet(&self, packet: &EncodedOpusPacket) -> bool {
        Self::supports_passthrough_packet(packet)
    }

    fn played_position(&self) -> Option<Duration> {
        let end_sample = self.played_end_sample.load(Ordering::Acquire);
        (end_sample != u64::MAX).then(|| {
            let rate = u64::from(PACKET_RATE);
            Duration::from_secs(end_sample / rate)
                + Duration::from_nanos((end_sample % rate) * 1_000_000_000 / rate)
        })
    }

    fn flush(
        &self,
        _position: Duration,
        cancellation: &CoreCancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        #[cfg(feature = "experimental-scheduler")]
        {
            lock(&self.pending_control).take();
        }
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
            .map_err(OutputError::with_source)?;
        // The next dispatched packet belongs to the new seek timeline.
        self.played_end_sample.store(u64::MAX, Ordering::Release);
        Ok(())
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
        #[cfg(feature = "experimental-scheduler")]
        self.inner.readiness.wake();
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

#[cfg(feature = "experimental-scheduler")]
mod readiness;
