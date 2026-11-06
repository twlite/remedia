use super::*;

#[test]
fn seeking_while_paused_flushes_output_and_discards_stale_frames() {
    let source = TempWave::from_samples(
        (0..96_000)
            .map(|index| if index < 48_000 { 1200 } else { 12_000 })
            .collect(),
    );
    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    pause_at_first_blocked_frame(&session, &gate);

    let target = Duration::from_secs(1);
    session.seek(target).unwrap();
    assert_eq!(session.position().submitted, target);
    assert_eq!(gate.snapshot().flushes, vec![target]);
    let accepted_before_seek_resume = gate.snapshot().accepted.len();

    session.resume().unwrap();
    assert!(gate.wait_for_calls(accepted_before_seek_resume + 1));
    gate.release(1);
    assert!(gate.wait_for_accepted(accepted_before_seek_resume + 1));
    let snapshot = gate.snapshot();
    let (start_sample, first_sample) = snapshot.accepted[accepted_before_seek_resume];
    assert!(start_sample >= 48_000);
    assert!(
        first_sample > 0.2,
        "post-seek audio should come from the second half"
    );

    session.stop().unwrap();
}

#[test]
fn stop_and_seek_can_race_without_deadlock_or_stale_delivery() {
    let source = TempWave::constant(96_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));

    let seeking_session = Arc::clone(&session);
    let seek_thread = thread::spawn(move || seeking_session.seek(Duration::from_secs(1)));
    assert!(wait_for_pending_seek(&session));
    session.stop().unwrap();
    assert!(matches!(
        seek_thread.join().unwrap(),
        Err(PlaybackError::Cancelled)
    ));

    let snapshot = gate.snapshot();
    assert_eq!(session.state(), PlaybackState::Stopped);
    assert!(snapshot.cancelled);
    assert!(snapshot.accepted.is_empty());
    assert!(lock(&session.worker).is_none());
}
