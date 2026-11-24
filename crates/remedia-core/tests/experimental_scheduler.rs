#![cfg(feature = "experimental-scheduler")]
use remedia_core::*;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn drain(reader: &mut StreamReader) -> Vec<(u64, u32, Vec<u8>)> {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut chunks = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "stream stalled");
        match reader.poll_next(&mut cx) {
            Poll::Ready(Ok(Some(chunk))) => chunks.push((
                chunk.start_sample,
                chunk.media_samples_per_channel,
                chunk.data,
            )),
            Poll::Ready(Ok(None)) => return chunks,
            Poll::Ready(Err(error)) => panic!("stream failed: {error}"),
            Poll::Pending => std::thread::park_timeout(Duration::from_millis(100)),
        }
    }
}

fn source() -> InputSource {
    InputSource::bytes(include_bytes!("fixtures/generated-tone.mp3").to_vec())
}

#[test]
fn scheduled_streams_match_dedicated_payloads_and_sample_positions() {
    let pool = PlaybackScheduler::new(2, 64).unwrap();
    for format in [StreamFormat::S16le, StreamFormat::F32le, StreamFormat::Opus] {
        let mut outputs = Vec::new();
        for scheduled in [false, true] {
            let session = if scheduled {
                PlaybackSession::with_scheduler(pool.clone())
            } else {
                PlaybackSession::new()
            };
            let (output, mut reader) =
                StreamOutput::new(format, StreamPacing::Unpaced, OpusEncoderConfig::default())
                    .unwrap();
            session.play(source(), output).unwrap();
            outputs.push(drain(&mut reader));
            assert_eq!(session.wait(), PlaybackState::Completed);
            session.stop().unwrap();
            if scheduled {
                assert_eq!(session.executor_diagnostic(), "scheduled");
            }
        }
        assert!(!outputs[0].is_empty());
        assert_eq!(outputs[0], outputs[1]);
    }
    assert_eq!(pool.diagnostics().sessions, 0);
}

#[test]
fn stalled_consumers_cancel_without_starving_unrelated_sessions() {
    let pool = PlaybackScheduler::new(1, 64).unwrap();
    let mut stalled = Vec::new();
    for _ in 0..32 {
        let session = PlaybackSession::with_scheduler(pool.clone());
        let (output, reader) = StreamOutput::new(
            StreamFormat::F32le,
            StreamPacing::Unpaced,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session.play(source(), output).unwrap();
        stalled.push((session, reader));
    }
    let session = PlaybackSession::with_scheduler(pool.clone());
    let output = NullOutput::new();
    let stats = output.stats_handle();
    session.play(source(), output).unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    session.stop().unwrap();
    assert!(stats.snapshot().sample_frames_received > 0);
    for (session, reader) in stalled {
        session.pause().unwrap();
        session.seek(Duration::from_millis(20)).unwrap();
        session.resume().unwrap();
        session.stop().unwrap();
        drop(reader);
    }
    assert_eq!(pool.diagnostics().sessions, 0);
    assert_eq!(pool.diagnostics().queued, 0);
}

#[test]
fn unsupported_inputs_and_capacity_exhaustion_report_fallback() {
    assert!(PlaybackScheduler::new(0, 1).is_err());
    let pool = PlaybackScheduler::new(1, 1).unwrap();
    let stalled = PlaybackSession::with_scheduler(pool.clone());
    let (output, reader) = StreamOutput::new(
        StreamFormat::F32le,
        StreamPacing::Unpaced,
        OpusEncoderConfig::default(),
    )
    .unwrap();
    stalled.play(source(), output).unwrap();
    // Startup returns before CPU-pool admission, so wait for its explicit diagnostic.
    let deadline = Instant::now() + Duration::from_secs(5);
    while stalled.executor_diagnostic() != "scheduled" {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let other = PlaybackSession::with_scheduler(pool.clone());
    other.play(source(), NullOutput::new()).unwrap();
    assert_eq!(other.wait(), PlaybackState::Completed);
    other.stop().unwrap();
    assert_eq!(other.executor_diagnostic(), "dedicated: pool capacity");
    stalled.stop().unwrap();
    drop(reader);
    other
        .play(
            InputSource::file(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/generated-tone.mp3"
            )),
            NullOutput::new(),
        )
        .unwrap();
    assert_eq!(other.wait(), PlaybackState::Completed);
    other.stop().unwrap();
    assert_eq!(
        other.executor_diagnostic(),
        "dedicated: unsupported workload"
    );
}

#[test]
fn repeated_generations_and_external_cancellation_release_pool_slots() {
    let pool = PlaybackScheduler::new(2, 8).unwrap();
    let session = PlaybackSession::with_scheduler(pool.clone());
    for generation in 0..128 {
        let token = CancellationToken::new();
        let (output, reader) = StreamOutput::new(
            StreamFormat::F32le,
            StreamPacing::Unpaced,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session
            .play_with_cancellation(source(), output, token.clone())
            .unwrap();
        if generation % 2 == 0 {
            token.cancel();
        }
        drop(reader);
        session.stop().unwrap();
        assert_eq!(pool.diagnostics().sessions, 0);
        assert_eq!(pool.diagnostics().queued, 0);
    }
}
