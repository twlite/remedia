use super::*;
use remedia_core::OutputControl;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

type BarrierFuture = Pin<Box<dyn Future<Output = Result<(), OutputError>> + Send>>;
pub(crate) struct PendingControl {
    kind: OutputControl,
    future: BarrierFuture,
}

impl DiscordOpusSender {
    pub(crate) fn poll_packet(
        &self,
        packet: &mut Option<EncodedOpusPacket>,
        cx: &mut Context<'_>,
        cancellation: &CoreCancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        // Register before checking capacity, including when a consumer drains concurrently.
        self.inner.readiness.register(cx.waker());
        let generation = self
            .ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        let value = packet
            .as_ref()
            .ok_or_else(|| OutputError::new("missing pending Opus packet"))?;
        validate_opus_packet(value).map_err(OutputError::with_source)?;
        let id = self
            .inner
            .next_packet_id
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let queued = QueuedPacket {
            generation,
            id,
            packet: packet.take().expect("validated packet"),
            played_end_sample: self.played_end_sample.clone(),
        };
        match self.inner.packet_tx.try_send(queued) {
            Ok(()) => {
                self.inner
                    .accepted_packet_id
                    .fetch_max(id, Ordering::AcqRel);
                Poll::Ready(Ok(()))
            }
            Err(flume::TrySendError::Full(returned)) => {
                *packet = Some(returned.packet);
                Poll::Pending
            }
            Err(flume::TrySendError::Disconnected(returned)) => {
                *packet = Some(returned.packet);
                Poll::Ready(Err(OutputError::with_source(VoiceError::Disconnected)))
            }
        }
    }

    pub(crate) fn poll_control(
        &self,
        kind: OutputControl,
        cx: &mut Context<'_>,
        cancellation: &CoreCancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        self.inner.readiness.register(cx.waker());
        let generation = self
            .ensure_current(cancellation)
            .map_err(OutputError::with_source)?;
        let mut slot = lock(&self.pending_control);
        if slot.as_ref().is_some_and(|pending| pending.kind != kind) {
            if matches!(kind, OutputControl::Flush(_))
                || (slot
                    .as_ref()
                    .is_some_and(|pending| pending.kind == OutputControl::Finish)
                    && kind == OutputControl::Pause)
            {
                // FIFO admission lets pause replace a draining finish barrier. Resume will
                // request a fresh EOF acknowledgement without encoding the tail twice.
                slot.take();
            } else {
                return Poll::Ready(Err(OutputError::new("overlapping Discord output barriers")));
            }
        }
        let generation = if slot.is_none() && matches!(kind, OutputControl::Flush(_)) {
            self.begin_flush()?
        } else {
            generation
        };
        let pending = slot.get_or_insert_with(|| {
            let (ack, receiver) = flume::bounded(1);
            let target_id = self.inner.accepted_packet_id.load(Ordering::Acquire);
            let control = match kind {
                OutputControl::Pause => Control::Pause {
                    generation,
                    target_id,
                    ack,
                },
                OutputControl::Resume => Control::Resume { generation, ack },
                OutputControl::Flush(_) => Control::Flush { generation, ack },
                OutputControl::Finish => Control::Finish {
                    generation,
                    target_id,
                    ack,
                },
            };
            let sender = self.inner.control_tx.clone();
            PendingControl {
                kind,
                future: Box::pin(async move {
                    sender
                        .send_async(control)
                        .await
                        .map_err(|_| OutputError::with_source(VoiceError::Disconnected))?;
                    receiver
                        .recv_async()
                        .await
                        .map_err(|_| OutputError::with_source(VoiceError::Disconnected))?
                        .map_err(OutputError::with_source)
                }),
            }
        });
        let result = pending.future.as_mut().poll(cx);
        if result.is_ready() {
            lock_free_finish(self, kind, &result);
        }
        // End the borrow before removing the completed future.
        if result.is_ready() {
            slot.take();
        }
        result
    }
}

fn lock_free_finish(
    sender: &DiscordOpusSender,
    kind: OutputControl,
    result: &Poll<Result<(), OutputError>>,
) {
    if kind == OutputControl::Finish && matches!(result, Poll::Ready(Ok(()))) {
        sender.finished.store(true, Ordering::Release);
    }
    if matches!(kind, OutputControl::Flush(_)) && matches!(result, Poll::Ready(Ok(()))) {
        sender.played_end_sample.store(u64::MAX, Ordering::Release);
    }
}
