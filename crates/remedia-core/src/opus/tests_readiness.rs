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
    allow_sync: Arc<AtomicBool>,
    wake: Arc<Mutex<Option<Waker>>>,
}

impl OpusPacketSink for ReadySink {
    fn scheduler_nonblocking(&self) -> bool {
        true
    }

    fn scheduler_poll_packet(
        &self,
        packet: &mut Option<EncodedOpusPacket>,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        *lock(&self.wake) = Some(cx.waker().clone());
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

    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        assert!(
            self.allow_sync.load(Ordering::Acquire),
            "scheduled output called blocking send"
        );
        self.capture.send_packet(packet, cancellation)
    }

    fn flush(&self, _: Duration, _: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        assert!(
            self.allow_sync.load(Ordering::Acquire),
            "scheduled output called blocking finish"
        );
        self.capture.finish(cancellation)
    }

    fn cancel(&self) {
        if let Some(wake) = lock(&self.wake).take() {
            wake.wake();
        }
    }
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

#[test]
fn unsupported_filter_fallback_drains_retained_pcm_before_synchronous_delivery() {
    use crate::{InputSource, PlaybackScheduler, PlaybackSession, PlaybackState};
    use std::time::Instant;
    let pool = PlaybackScheduler::new(1, 8).unwrap();
    let session = Arc::new(PlaybackSession::with_scheduler(pool.clone()));
    let sink = ReadySink::default();
    sink.allow_sync.store(true, Ordering::Release);
    sink.barrier_open.store(true, Ordering::Release);
    let source =
        || InputSource::bytes(include_bytes!("../../tests/fixtures/generated-tone.mp3").to_vec());
    session
        .play(source(), OpusEncoderOutput::new(sink.clone()).unwrap())
        .unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while lock(&sink.wake).is_none() {
        assert!(Instant::now() < limit);
        std::thread::yield_now();
    }
    let updating = session.clone();
    let update = std::thread::spawn(move || {
        updating.set_filter_graph(Some(if cfg!(windows) {
            "ametadata=mode=print:file=NUL"
        } else {
            "ametadata=mode=print:file=/dev/null"
        }))
    });
    while session.executor_diagnostic() != "dedicated: unsupported filter" {
        assert!(Instant::now() < limit);
        std::thread::yield_now();
    }
    let other = PlaybackSession::with_scheduler(pool.clone());
    other.play(source(), crate::NullOutput::new()).unwrap();
    assert_eq!(other.wait(), PlaybackState::Completed);
    other.stop().unwrap();
    sink.packets_open.store(true, Ordering::Release);
    if let Some(wake) = lock(&sink.wake).take() {
        wake.wake();
    }
    update.join().unwrap().unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    session.stop().unwrap();
    let capture = CaptureSink::default();
    let reference = PlaybackSession::new();
    reference
        .play(source(), OpusEncoderOutput::new(capture.clone()).unwrap())
        .unwrap();
    assert_eq!(reference.wait(), PlaybackState::Completed);
    reference.stop().unwrap();
    assert_eq!(lock(&sink.capture.0).packets, lock(&capture.0).packets);
    assert_eq!(pool.diagnostics().control_fallbacks, 1);
}
