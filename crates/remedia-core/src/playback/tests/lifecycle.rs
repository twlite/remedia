use super::*;

#[test]
fn null_output_reports_natural_completion_and_position() {
    let source = TempWave::constant(4800, 4096);
    let session = PlaybackSession::new();
    let output = NullOutput::new();
    let stats = output.stats_handle();

    let info = session.play(source.source(), output).unwrap();
    assert_eq!(info.duration, Some(Duration::from_millis(100)));
    assert_eq!(session.wait(), PlaybackState::Completed);

    let output_stats: NullOutputStats = stats.snapshot();
    assert!(output_stats.frames_received > 0);
    assert_eq!(output_stats.samples_received, 9600);
    assert_eq!(output_stats.sample_frames_received, 4800);
    assert_eq!(output_stats.duration, Duration::from_millis(100));
    assert!(output_stats.finished_successfully);
    assert_eq!(session.duration(), Some(Duration::from_millis(100)));
    assert_eq!(session.position().submitted, Duration::from_millis(100));
    assert_eq!(session.position().played, Some(Duration::from_millis(100)));
    assert_eq!(
        session.position().media.map(|position| position.position),
        Some(Duration::from_millis(100))
    );
    session.stop().unwrap();
    assert_eq!(session.state(), PlaybackState::Completed);
}

#[test]
fn completion_waiter_remains_bound_to_its_source_after_replacement() {
    let source = TempWave::constant(48_000, 4096);
    let session = PlaybackSession::new();
    let (first_output, first_gate) = GateOutput::new();
    session.play(source.source(), first_output).unwrap();
    assert!(first_gate.wait_for_calls(1));
    let first_waiter = session.completion();

    session.stop().unwrap();
    let (second_output, second_gate) = GateOutput::new();
    session.play(source.source(), second_output).unwrap();
    assert!(second_gate.wait_for_calls(1));

    assert_eq!(first_waiter.wait(), PlaybackState::Stopped);
    assert_eq!(session.state(), PlaybackState::Playing);
    session.stop().unwrap();
}

#[test]
fn pause_resume_stop_and_repeated_operation_transitions_are_typed() {
    let source = TempWave::constant(96_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    assert!(matches!(
        session.pause(),
        Err(PlaybackError::InvalidState { .. })
    ));
    assert!(matches!(
        session.resume(),
        Err(PlaybackError::InvalidState { .. })
    ));
    session.stop().unwrap();
    session.stop().unwrap();
    assert_eq!(session.state(), PlaybackState::Stopped);

    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(matches!(
        session.play(source.source(), NullOutput::new()),
        Err(PlaybackError::InvalidState { .. })
    ));
    pause_at_first_blocked_frame(&session, &gate);
    session.pause().unwrap();
    assert_eq!(session.state(), PlaybackState::Paused);
    session.resume().unwrap();
    session.resume().unwrap();
    assert!(gate.wait_for_calls(2));
    session.stop().unwrap();
    session.stop().unwrap();
    assert_eq!(session.state(), PlaybackState::Stopped);
    assert!(gate.snapshot().cancelled);
    assert!(lock(&session.worker).is_none());
}

#[test]
fn stop_during_paused_playback_releases_the_worker() {
    let source = TempWave::constant(48_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    pause_at_first_blocked_frame(&session, &gate);

    session.stop().unwrap();

    assert_eq!(session.state(), PlaybackState::Stopped);
    assert!(gate.snapshot().cancelled);
    assert!(lock(&session.worker).is_none());
}

#[test]
fn concurrent_stop_calls_wait_for_cleanup_and_both_succeed() {
    let source = TempWave::constant(48_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));

    let barrier = Arc::new(Barrier::new(3));
    let first_session = Arc::clone(&session);
    let first_barrier = Arc::clone(&barrier);
    let first_stop = thread::spawn(move || {
        first_barrier.wait();
        first_session.stop()
    });
    let second_session = Arc::clone(&session);
    let second_barrier = Arc::clone(&barrier);
    let second_stop = thread::spawn(move || {
        second_barrier.wait();
        second_session.stop()
    });

    barrier.wait();
    assert!(first_stop.join().unwrap().is_ok());
    assert!(second_stop.join().unwrap().is_ok());
    assert_eq!(session.state(), PlaybackState::Stopped);
    assert!(gate.snapshot().cancelled);
    assert!(lock(&session.worker).is_none());
}

#[test]
fn blocked_output_applies_backpressure_without_a_session_audio_queue() {
    let source = TempWave::constant(96_000, 4096);
    let session = PlaybackSession::new();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));

    let blocked = gate.snapshot();
    assert_eq!(blocked.write_calls, 1);
    assert!(blocked.accepted.is_empty());
    assert_eq!(session.position().submitted, Duration::ZERO);

    session.stop().unwrap();
    let stopped = gate.snapshot();
    assert_eq!(stopped.write_calls, 1);
    assert!(stopped.cancelled);
}

#[test]
fn pre_cancelled_play_does_not_start_a_source() {
    let session = PlaybackSession::new();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = session.play_with_cancellation(
        InputSource::file("missing.wav"),
        NullOutput::new(),
        cancellation,
    );

    assert!(matches!(result, Err(PlaybackError::Cancelled)));
    assert_eq!(session.state(), PlaybackState::Idle);
}

#[test]
fn decoder_and_output_failures_reach_the_session() {
    let invalid_directory =
        std::env::temp_dir().join(format!("remedia-playback-invalid-{}", std::process::id()));
    fs::create_dir_all(&invalid_directory).unwrap();
    let invalid_path = invalid_directory.join("invalid.bin");
    fs::write(&invalid_path, b"not a media file").unwrap();

    let decoder_session = PlaybackSession::new();
    let (decoder_error_output, decoder_output_handle) = GateOutput::new();
    assert!(matches!(
        decoder_session.play(InputSource::file(&invalid_path), decoder_error_output),
        Err(PlaybackError::Decoder(_))
    ));
    assert_eq!(decoder_session.state(), PlaybackState::Failed);
    assert!(decoder_output_handle.snapshot().cancelled);
    assert!(matches!(
        decoder_session.last_error(),
        Some(PlaybackError::Decoder(_))
    ));
    decoder_session.stop().unwrap();
    let _ = fs::remove_dir_all(&invalid_directory);

    let source = TempWave::constant(48_000, 4096);
    let output_cancelled = Arc::new(AtomicBool::new(false));
    let output_session = PlaybackSession::new();
    output_session
        .play(
            source.source(),
            FailingOutput(Arc::clone(&output_cancelled)),
        )
        .unwrap();
    assert_eq!(output_session.wait(), PlaybackState::Failed);
    assert!(matches!(
        output_session.last_error(),
        Some(PlaybackError::Output(_))
    ));
    assert!(output_cancelled.load(Ordering::Acquire));
    output_session.stop().unwrap();
}

#[test]
fn separate_sessions_progress_independently_under_backpressure() {
    let source_a = TempWave::constant(24_000, 3000);
    let source_b = TempWave::constant(24_000, 6000);
    let observability = Observability::new(
        ObservabilityConfig::default()
            .with_filter("info")
            .expect("valid test filter"),
    )
    .expect("create observability context");
    let diagnostics = observability.diagnostics();
    let handle = observability.handle();
    let session_a = PlaybackSession::with_observability(handle.clone());
    let session_b = PlaybackSession::with_observability(handle);
    let (output_a, gate_a) = GateOutput::new();
    let (output_b, gate_b) = GateOutput::new();

    session_a.play(source_a.source(), output_a).unwrap();
    session_b.play(source_b.source(), output_b).unwrap();
    let mut playback_ids = std::collections::HashSet::new();
    let mut events_seen = Vec::new();
    while playback_ids.len() < 2 {
        let event = diagnostics
            .recv_timeout(TEST_TIMEOUT)
            .unwrap_or_else(|error| {
                panic!("playback start diagnostics arrive; saw events {events_seen:?}: {error}")
            });
        events_seen.push(event.event.clone());
        if event.event == "core.playback.starting" {
            playback_ids.insert(event.playback_id.expect("playback ID field"));
        }
    }
    assert_eq!(
        playback_ids.len(),
        2,
        "sessions must have distinct correlation IDs"
    );
    assert!(gate_a.wait_for_calls(1));
    assert!(gate_b.wait_for_calls(1));
    assert!(gate_a.snapshot().accepted.is_empty());
    assert!(gate_b.snapshot().accepted.is_empty());

    gate_a.release(usize::MAX);
    assert_eq!(session_a.wait(), PlaybackState::Completed);
    assert!(gate_a.snapshot().finished);
    assert_eq!(session_b.state(), PlaybackState::Playing);
    assert_eq!(gate_b.snapshot().write_calls, 1);
    assert!(gate_b.snapshot().accepted.is_empty());
    assert_eq!(session_b.position().submitted, Duration::ZERO);

    gate_b.release(usize::MAX);
    assert_eq!(session_b.wait(), PlaybackState::Completed);
    session_a.stop().unwrap();
    session_b.stop().unwrap();
}

#[test]
fn stop_then_play_replaces_a_source_without_old_frames_after_stop() {
    let old_source = TempWave::constant(96_000, 1000);
    let new_source = TempWave::constant(4800, 12_000);
    let session = PlaybackSession::new();
    let (old_output, old_gate) = GateOutput::new();
    session.play(old_source.source(), old_output).unwrap();
    assert!(old_gate.wait_for_calls(1));
    old_gate.release(1);
    assert!(old_gate.wait_for_accepted(1));
    assert!(old_gate.wait_for_calls(2));

    session.stop().unwrap();
    let old_snapshot_at_stop = old_gate.snapshot();
    assert_eq!(old_snapshot_at_stop.accepted.len(), 1);
    assert!(old_snapshot_at_stop.cancelled);

    let output = NullOutput::new();
    let stats = output.stats_handle();
    session.play(new_source.source(), output).unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(stats.snapshot().sample_frames_received, 4800);
    assert_eq!(
        old_gate.snapshot().accepted.len(),
        old_snapshot_at_stop.accepted.len()
    );
    session.stop().unwrap();
}
