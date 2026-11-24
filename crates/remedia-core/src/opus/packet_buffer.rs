//! Retain the current production burst until the readiness-aware sink admits it.
//! Normal PCM production is sliced into one encoder frame; only finalization can emit two.
use super::*;

pub(super) struct PacketBuffer {
    pub(super) inner: Box<dyn OpusPacketSink>,
    #[cfg(feature = "experimental-scheduler")]
    enabled: AtomicBool,
    #[cfg(feature = "experimental-scheduler")]
    pub(super) pending: Mutex<std::collections::VecDeque<EncodedOpusPacket>>,
}

impl PacketBuffer {
    pub(super) fn new(inner: Box<dyn OpusPacketSink>) -> Self {
        Self {
            inner,
            #[cfg(feature = "experimental-scheduler")]
            enabled: AtomicBool::new(false),
            #[cfg(feature = "experimental-scheduler")]
            pending: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    #[cfg(feature = "experimental-scheduler")]
    pub(super) fn active(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    #[cfg(feature = "experimental-scheduler")]
    pub(super) fn activate(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    #[cfg(feature = "experimental-scheduler")]
    pub(super) fn poll(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        let mut pending = lock(&self.pending);
        while let Some(packet) = pending.pop_front() {
            let mut packet = Some(packet);
            let result = self
                .inner
                .scheduler_poll_packet(&mut packet, cx, cancellation);
            if let Some(packet) = packet {
                pending.push_front(packet);
            }
            match result {
                std::task::Poll::Ready(Ok(())) => {}
                other => return other,
            }
        }
        std::task::Poll::Ready(Ok(()))
    }
}

impl OpusPacketSink for PacketBuffer {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        #[cfg(feature = "experimental-scheduler")]
        if self.active() {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let mut pending = lock(&self.pending);
            if pending.len() >= 2 {
                return Err(OutputError::new(
                    "Opus production exceeded its two-packet burst",
                ));
            }
            pending.push_back(packet);
            return Ok(());
        }
        self.inner.send_packet(packet, cancellation)
    }

    fn supports_passthrough_packet(&self, packet: &EncodedOpusPacket) -> bool {
        self.inner.supports_passthrough_packet(packet)
    }

    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.flush_samples(duration_to_samples(position)?, cancellation)
    }

    fn flush_samples(
        &self,
        position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        #[cfg(feature = "experimental-scheduler")]
        {
            lock(&self.pending).clear();
        }
        self.inner.flush_samples(position, cancellation)
    }

    fn pause(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.inner.pause(cancellation)
    }

    fn resume(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.inner.resume(cancellation)
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.inner.finish(cancellation)
    }

    fn cancel(&self) {
        // The sink wake must precede taking the local buffer lock: a caller may be inside a poll.
        self.inner.cancel();
        #[cfg(feature = "experimental-scheduler")]
        {
            lock(&self.pending).clear();
        }
    }

    fn played_position(&self) -> Option<Duration> {
        self.inner.played_position()
    }
}
