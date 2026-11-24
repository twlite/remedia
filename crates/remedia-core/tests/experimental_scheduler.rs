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
                assert!(session.executor_diagnostic().starts_with("scheduled"));
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
        assert!(session.executor_diagnostic().starts_with("scheduled"));
        session.set_filter_graph(Some("volume=0.75")).unwrap();
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
            InputSource::bytes(include_bytes!("fixtures/ffmpeg-opus-silent-lead.webm").to_vec()),
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

#[test]
fn live_filter_returns_to_cpu_workers_after_isolated_replacement() {
    let pool = PlaybackScheduler::new(1, 8).unwrap();
    let session = PlaybackSession::with_scheduler(pool.clone());
    let (output, reader) = StreamOutput::new(
        StreamFormat::F32le,
        StreamPacing::Unpaced,
        OpusEncoderConfig::default(),
    )
    .unwrap();
    session.play(source(), output).unwrap();
    session.pause().unwrap();
    session.set_filter_graph(Some("volume=0.75")).unwrap();
    assert!(session.executor_diagnostic().starts_with("scheduled"));
    session.stop().unwrap();
    drop(reader);
    assert_eq!(pool.diagnostics().sessions, 0);
}

#[test]
fn processing_budgets_preserve_audio_and_account_for_dispatches() {
    assert!(PlaybackScheduler::with_processing_budget(1, 8, 0).is_err());
    let mut reference = None;
    for budget in [1, 8, 32] {
        let pool = PlaybackScheduler::with_processing_budget(2, 8, budget).unwrap();
        let session = PlaybackSession::with_scheduler(pool.clone());
        let (output, mut reader) = StreamOutput::new(
            StreamFormat::F32le,
            StreamPacing::Unpaced,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session.play(source(), output).unwrap();
        let chunks = drain(&mut reader);
        session.stop().unwrap();
        if let Some(expected) = &reference {
            assert_eq!(&chunks, expected);
        } else {
            reference = Some(chunks);
        }
        let d = pool.diagnostics();
        assert_eq!(d.admitted, 1);
        assert_eq!(d.scheduled_sessions, 0);
        assert_eq!(
            d.control_fallbacks + d.unsupported_fallbacks + d.capacity_fallbacks,
            0
        );
        if budget == 1 {
            assert_eq!(d.turns, d.actions);
        } else {
            assert!(d.turns < d.actions);
        }
    }
}

#[test]
fn paced_streams_use_bounded_deadlines_and_preserve_payloads() {
    let mut reference = None;
    for budget in [1, 8, 32] {
        let pool = PlaybackScheduler::with_processing_budget(2, 8, budget).unwrap();
        let session = PlaybackSession::with_scheduler(pool.clone());
        let (output, mut reader) = StreamOutput::new(
            StreamFormat::Opus,
            StreamPacing::Realtime,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        let started = Instant::now();
        session.play(source(), output).unwrap();
        let chunks = drain(&mut reader);
        session.stop().unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "pacing must not become unpaced"
        );
        if let Some(expected) = &reference {
            assert_eq!(&chunks, expected);
        } else {
            reference = Some(chunks);
        }
        let d = pool.diagnostics();
        assert!(d.deadline_suspensions > 0);
        assert!(d.deadline_wakes > 0);
        assert_eq!(d.pending_deadlines, 0);
        assert!(d.deadline_peak <= 1);
        assert!(session.executor_diagnostic().starts_with("scheduled"));
    }
}

#[test]
fn cancelling_and_pausing_timed_sessions_reclaims_all_deadlines() {
    let pool = PlaybackScheduler::with_processing_budget(1, 64, 8).unwrap();
    let mut sessions = Vec::new();
    for _ in 0..32 {
        let session = PlaybackSession::with_scheduler(pool.clone());
        let (output, reader) = StreamOutput::new(
            StreamFormat::Opus,
            StreamPacing::Realtime,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session.play(source(), output).unwrap();
        sessions.push((session, reader));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while pool.diagnostics().deadline_suspensions == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    for (session, reader) in sessions {
        session.pause().unwrap();
        session.resume().unwrap();
        session.stop().unwrap();
        drop(reader);
    }
    let d = pool.diagnostics();
    assert_eq!(d.pending_deadlines, 0);
    assert_eq!(d.scheduled_sessions, 0);
    assert_eq!(d.sessions, 0);
    assert_eq!(d.queued, 0);
    assert!(d.deadline_peak <= 32);
}

#[test]
fn repeated_paused_controls_return_to_scheduler_and_match_dedicated_audio() {
    let pool = PlaybackScheduler::with_processing_budget(1, 16, 8).unwrap();
    let mut results = Vec::new();
    for scheduled in [false, true] {
        let session = if scheduled {
            PlaybackSession::with_scheduler(pool.clone())
        } else {
            PlaybackSession::new()
        };
        let (output, mut reader) = StreamOutput::new(
            StreamFormat::Opus,
            StreamPacing::Unpaced,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session.play(source(), output).unwrap();
        session.pause().unwrap();
        for index in 0..8 {
            session.seek(Duration::from_millis(20 + index * 5)).unwrap();
            session
                .set_filter_graph(Some(if index % 2 == 0 {
                    "volume=0.75"
                } else {
                    "volume=0.5"
                }))
                .unwrap();
            if scheduled {
                assert!(session.executor_diagnostic().starts_with("scheduled"));
            }
        }
        session.resume().unwrap();
        results.push(drain(&mut reader));
        assert_eq!(session.wait(), PlaybackState::Completed);
        session.stop().unwrap();
    }
    assert!(!results[0].is_empty());
    assert_eq!(results[0], results[1]);
    let diagnostics = pool.diagnostics();
    assert_eq!(diagnostics.admitted, 1);
    assert_eq!(diagnostics.control_handoffs, 16);
    assert_eq!(diagnostics.control_fallbacks, 0);
    assert_eq!(diagnostics.isolated_controls, 0);
    assert_eq!(diagnostics.sessions, 0);
}

#[test]
fn local_demux_queues_preserve_audio_controls_and_cleanup() {
    let pool = PlaybackScheduler::with_processing_budget(2, 64, 8).unwrap();
    for fixture in [
        "generated-tone.mp3",
        "generated-opus-passthrough-long.ogg",
        "generated-opus-trimmed.ogg",
    ] {
        for format in [StreamFormat::F32le, StreamFormat::Opus] {
            let mut results = Vec::new();
            for scheduled in [false, true] {
                let session = if scheduled {
                    PlaybackSession::with_scheduler(pool.clone())
                } else {
                    PlaybackSession::new()
                };
                let (output, mut reader) =
                    StreamOutput::new(format, StreamPacing::Unpaced, OpusEncoderConfig::default())
                        .unwrap();
                session
                    .play(
                        InputSource::file(
                            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                                .join("tests/fixtures")
                                .join(fixture),
                        ),
                        output,
                    )
                    .unwrap();
                session.pause().unwrap();
                for target in [20, 40, 30] {
                    let outcome = session.seek_outcome(Duration::from_millis(target)).unwrap();
                    assert_eq!(outcome.applied, Duration::from_millis(target));
                }
                session.set_filter_graph(Some("volume=0.75")).unwrap();
                session.set_filter_graph(None).unwrap();
                session.resume().unwrap();
                results.push(drain(&mut reader));
                assert_eq!(session.wait(), PlaybackState::Completed);
                session.stop().unwrap();
                if scheduled {
                    assert_eq!(session.executor_diagnostic(), "scheduled");
                }
            }
            assert!(!results[0].is_empty());
            assert_eq!(results[0], results[1], "{fixture} {format:?}");
        }
    }
    assert_eq!(pool.diagnostics().admitted, 6);
    assert_eq!(pool.diagnostics().unsupported_fallbacks, 0);
    assert_eq!(pool.diagnostics().sessions, 0);
    assert_eq!(pool.diagnostics().queued_io, 0);
}

#[test]
fn concurrent_file_cancellation_reclaims_bounded_demux_tasks() {
    let pool = PlaybackScheduler::new(1, 64).unwrap();
    let mut sessions = Vec::new();
    for _ in 0..32 {
        let session = PlaybackSession::with_scheduler(pool.clone());
        let token = CancellationToken::new();
        let (output, reader) = StreamOutput::new(
            StreamFormat::F32le,
            StreamPacing::Unpaced,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session
            .play_with_cancellation(
                InputSource::file(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/generated-tone.mp3"
                )),
                output,
                token.clone(),
            )
            .unwrap();
        sessions.push((session, token, reader));
    }
    for (session, token, reader) in sessions {
        token.cancel();
        session.stop().unwrap();
        drop(reader);
    }
    assert_eq!(pool.diagnostics().sessions, 0);
    assert_eq!(pool.diagnostics().queued_io, 0);
    assert_eq!(pool.diagnostics().queued_startups, 0);
}

#[test]
fn native_filters_with_io_capabilities_report_dedicated_fallbacks() {
    for initial in [true, false] {
        let pool = PlaybackScheduler::new(1, 8).unwrap();
        let session = PlaybackSession::with_scheduler(pool.clone());
        let graph = if cfg!(windows) {
            "ametadata=mode=print:file=NUL"
        } else {
            "ametadata=mode=print:file=/dev/null"
        };
        if initial {
            session.set_filter_graph(Some(graph)).unwrap();
        }
        let (output, mut reader) = StreamOutput::new(
            StreamFormat::F32le,
            StreamPacing::Unpaced,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        session.play(source(), output).unwrap();
        if !initial {
            session.pause().unwrap();
            session.set_filter_graph(Some(graph)).unwrap();
            assert_eq!(
                session.executor_diagnostic(),
                "dedicated: unsupported filter"
            );
            session.resume().unwrap();
        }
        assert!(!drain(&mut reader).is_empty());
        assert_eq!(session.wait(), PlaybackState::Completed);
        session.stop().unwrap();
        let d = pool.diagnostics();
        assert_eq!(d.filter_fallbacks, 1);
        assert_eq!(d.admitted, usize::from(!initial));
        assert_eq!(d.control_fallbacks, usize::from(!initial));
        assert_eq!(d.unsupported_fallbacks, usize::from(initial));
        assert_eq!(d.sessions, 0);
    }
}

#[test]
fn unsupported_container_probes_retry_on_a_tracked_dedicated_worker() {
    let pool = PlaybackScheduler::new(1, 8).unwrap();
    let session = PlaybackSession::with_scheduler(pool.clone());
    let (output, mut reader) = StreamOutput::new(
        StreamFormat::F32le,
        StreamPacing::Unpaced,
        OpusEncoderConfig::default(),
    )
    .unwrap();
    session
        .play(
            InputSource::bytes(include_bytes!("fixtures/ffmpeg-opus-silent-lead.webm").to_vec()),
            output,
        )
        .unwrap();
    assert!(!drain(&mut reader).is_empty());
    assert_eq!(session.wait(), PlaybackState::Completed);
    session.stop().unwrap();
    let d = pool.diagnostics();
    assert_eq!(d.probe_retries, 1);
    assert_eq!(d.admitted, 0);
    assert_eq!(d.demuxer_fallbacks, 1);
    assert_eq!(d.unsupported_fallbacks, 1);
    assert_eq!(
        session.executor_diagnostic(),
        "dedicated: unsupported workload"
    );
}
