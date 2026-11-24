use super::tests::{CaptureSink, assert_settings, pcm};
use super::*;
use crate::OutputControl;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

#[derive(Clone, Default)]
struct ReadySink {
    capture: CaptureSink,
    packets_open: Arc<AtomicBool>,
    barrier_open: Arc<AtomicBool>,
}

impl OpusPacketSink for ReadySink {
    fn scheduler_nonblocking(&self) -> bool {
        true
    }

    fn scheduler_poll_packet(
        &self,
        packet: &mut Option<EncodedOpusPacket>,
        _: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        if !self.packets_open.load(Ordering::Acquire) {
            return Poll::Pending;
        }
        Poll::Ready(
            self.capture
                .send_packet(packet.take().unwrap(), cancellation),
        )
    }

    fn scheduler_poll_control(
        &self,
        control: OutputControl,
        _: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        if !self.barrier_open.load(Ordering::Acquire) {
            return Poll::Pending;
        }
        assert_eq!(control, OutputControl::Finish);
        Poll::Ready(self.capture.finish(cancellation))
    }

    fn send_packet(&self, _: EncodedOpusPacket, _: &CancellationToken) -> Result<(), OutputError> {
        panic!("scheduled output called blocking send")
    }

    fn flush(&self, _: Duration, _: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    fn finish(&self, _: &CancellationToken) -> Result<(), OutputError> {
        panic!("scheduled output called blocking finish")
    }

    fn cancel(&self) {}
}

#[test]
fn scheduled_encoding_retains_bounded_packets_and_matches_dedicated_bytes() {
    let capture = CaptureSink::default();
    let dedicated = OpusEncoderOutput::new(capture.clone()).unwrap();
    let sink = ReadySink::default();
    let scheduled = OpusEncoderOutput::new(sink.clone()).unwrap();
    let token = CancellationToken::new();
    let mut cx = Context::from_waker(Waker::noop());
    scheduled.scheduler_activate();
    // A large input frame must not turn into a large pending compressed queue.
    dedicated
        .write_frame(pcm(0, 960 * 40 + 193), &token)
        .unwrap();
    scheduled
        .write_frame(pcm(0, 960 * 40 + 193), &token)
        .unwrap();
    assert!(scheduled.scheduler_poll_ready(&mut cx, &token).is_pending());
    assert_eq!(lock(&scheduled.sink.pending).len(), 1);
    assert!(lock(&sink.capture.0).packets.is_empty());
    sink.packets_open.store(true, Ordering::Release);
    let mut ready = false;
    for _ in 0..100 {
        if let Poll::Ready(result) = scheduled.scheduler_poll_ready(&mut cx, &token) {
            result.unwrap();
            ready = true;
            break;
        }
    }
    assert!(ready);
    dedicated.finish(&token).unwrap();
    assert!(
        scheduled
            .scheduler_poll_control(OutputControl::Finish, &mut cx, &token)
            .is_pending()
    );
    assert!(!lock(&sink.capture.0).finished);
    sink.barrier_open.store(true, Ordering::Release);
    assert!(matches!(
        scheduled.scheduler_poll_control(OutputControl::Finish, &mut cx, &token),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(lock(&capture.0).packets, lock(&sink.capture.0).packets);
    assert_settings(&scheduled, OpusEncoderConfig::default());
}
