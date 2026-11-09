use super::*;

#[test]
fn source_media_position_survives_tempo_and_speed_filters() {
    for (graph, expected_output_samples) in [
        ("volume=0.5", 48_000),
        ("asetrate=60000,aresample=48000", 38_400),
        ("asetrate=38400,aresample=48000", 60_000),
        ("atempo=1.25", 38_400),
    ] {
        let source = TempWave::constant(48_000, 4_096);
        let session = PlaybackSession::new();
        session.set_filter_graph(Some(graph)).unwrap();
        let output = NullOutput::new();
        session.play(source.source(), output).unwrap();
        assert_eq!(session.wait(), PlaybackState::Completed, "{graph}");

        let position = session.position();
        assert_eq!(
            position.media.map(|media| media.position),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            position.media.map(|media| media.accuracy),
            Some(MediaPositionAccuracy::Estimated)
        );
        assert!(
            (position.submitted.as_millis() as i64
                - expected_output_samples as i64 * 1_000 / 48_000)
                .abs()
                <= 3,
            "{graph}: {position:?}"
        );
        session.stop().unwrap();
    }
}

#[test]
fn mp3_skip_and_discard_samples_keep_the_media_position_near_track_end() {
    let session = PlaybackSession::new();
    let info = session
        .play(
            InputSource::bytes(
                include_bytes!("../../../../tests/fixtures/generated-tone.mp3").to_vec(),
            ),
            NullOutput::new(),
        )
        .expect("bundled MP3 opens");
    assert_eq!(session.wait(), PlaybackState::Completed);
    let expected_duration = info.duration.expect("fixture duration is known");
    let media_position = session
        .position()
        .media
        .expect("decoded frames report their media position")
        .position;
    assert!(
        media_position.abs_diff(expected_duration) < Duration::from_millis(100),
        "final media position {media_position:?}, track duration {expected_duration:?}"
    );
    session.stop().unwrap();
}

#[test]
fn live_filter_replacement_drops_old_graph_delay_and_keeps_master_gain_independent() {
    let source = TempWave::constant(96_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    session.set_volume(0.5).unwrap();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));

    let update_session = Arc::clone(&session);
    let (updated, updated_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = updated.send(update_session.set_filter_graph(Some("volume=0.25")));
    });
    assert!(wait_for_pending_filter_update(&session));
    session.set_volume(0.0).unwrap();
    gate.release(1);
    let update_result = updated_rx.recv_timeout(TEST_TIMEOUT);
    assert!(
        update_result.is_ok(),
        "filter replacement timed out: state={:?}, error={:?}, output={:?}",
        session.state(),
        session.last_error(),
        gate.snapshot()
    );
    assert!(update_result.unwrap().is_ok());
    assert_eq!(session.filter_graph().as_deref(), Some("volume=0.25"));
    assert!(gate.wait_for_calls(2));
    session.set_volume(1.5).unwrap();
    gate.release(1);
    assert!(gate.wait_for_calls(3));
    gate.release(1);
    assert!(gate.wait_for_accepted(3));

    let snapshot = gate.snapshot();
    let input_level = 4096.0 / 32768.0 / std::f32::consts::SQRT_2;
    assert!((snapshot.accepted[0].1 - input_level * 0.5).abs() < 0.001);
    assert!(snapshot.accepted[1].1.abs() < 0.001);
    assert!((snapshot.accepted[2].1 - input_level * 0.25 * 1.5).abs() < 0.001);
    assert!(snapshot.flushes.is_empty());
    assert_eq!(session.volume(), 1.5);

    assert!(matches!(
        session.set_filter_graph(Some("missing_filter=1")),
        Err(PlaybackError::FilterGraph(_))
    ));
    assert_eq!(session.filter_graph().as_deref(), Some("volume=0.25"));
    session.stop().unwrap();
}

#[test]
fn filter_and_seek_requests_racing_with_eof_are_resolved() {
    let source = TempWave::constant(960, 4096);
    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::blocked_finish();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    gate.release(usize::MAX);
    assert!(gate.wait_for_finish());

    let filter_session = Arc::clone(&session);
    let (filter_tx, filter_rx) = mpsc::sync_channel(1);
    let filter_thread = thread::spawn(move || {
        let _ = filter_tx.send(filter_session.set_filter_graph(Some("tremolo=f=5:d=0.6")));
    });
    assert!(wait_for_pending_filter_update(&session));

    let seek_session = Arc::clone(&session);
    let (seek_tx, seek_rx) = mpsc::sync_channel(1);
    let seek_thread = thread::spawn(move || {
        let _ = seek_tx.send(seek_session.seek(Duration::from_millis(5)));
    });
    assert!(wait_for_pending_seek(&session));

    gate.release_finish();
    assert!(
        filter_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("filter config is acknowledged after EOF")
            .is_ok()
    );
    assert!(matches!(
        seek_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("pending seek is resolved after EOF"),
        Err(PlaybackError::InvalidState {
            operation: "seek",
            state: PlaybackState::Completed,
        })
    ));
    filter_thread.join().unwrap();
    seek_thread.join().unwrap();
    assert_eq!(session.state(), PlaybackState::Completed);
    assert_eq!(session.filter_graph().as_deref(), Some("tremolo=f=5:d=0.6"));
    session.stop().unwrap();
}

#[test]
fn native_opus_filter_changes_keep_playback_active_and_packet_timeline_contiguous() {
    let source = TempWave::constant(960_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    session.set_volume(0.75).unwrap();
    let (sink, gate) = StepPacketSink::new();
    session
        .play(
            source.source(),
            OpusEncoderOutput::new(sink).expect("native Opus encoder initializes"),
        )
        .unwrap();
    assert!(
        gate.wait_for_calls(1),
        "Opus output reaches its first packet"
    );

    replace_graph_while_packet_sender_is_blocked(
        Arc::clone(&session),
        gate.clone(),
        Some("tremolo=f=5:d=0.6"),
    );
    assert_eq!(session.state(), PlaybackState::Playing);
    let mut latest_media_position = session
        .position()
        .media
        .expect("first accepted Opus output has a source position")
        .position;
    replace_graph_while_packet_sender_is_blocked(
        Arc::clone(&session),
        gate.clone(),
        Some("atempo=1.25"),
    );
    assert_eq!(session.state(), PlaybackState::Playing);
    let media_position = session
        .position()
        .media
        .expect("tempo output retains its source position")
        .position;
    assert!(media_position >= latest_media_position);
    latest_media_position = media_position;
    replace_graph_while_packet_sender_is_blocked(Arc::clone(&session), gate.clone(), None);
    assert_eq!(session.state(), PlaybackState::Playing);
    assert_eq!(session.volume(), 0.75);
    assert!(
        session
            .position()
            .media
            .expect("cleared graph retains source progress")
            .position
            >= latest_media_position
    );

    gate.release(usize::MAX);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(
        session.position().media.map(|media| media.position),
        Some(Duration::from_secs(20))
    );
    let snapshot = gate.snapshot();
    assert!(snapshot.finished);
    assert_eq!(snapshot.flushes, 0, "filter changes do not seek-flush Opus");
    assert!(snapshot.packets.len() > 10);
    for (index, packet) in snapshot.packets.iter().enumerate() {
        assert_eq!(packet.start_sample, index as u64 * 960);
        assert_eq!(packet.samples_per_channel, 960);
        assert!(packet.media_samples_per_channel <= 960);
    }
    session.stop().unwrap();
}

#[test]
fn stop_cancels_a_pending_native_opus_filter_replacement() {
    let source = TempWave::constant(48_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    let (sink, gate) = StepPacketSink::new();
    session
        .play(
            source.source(),
            OpusEncoderOutput::new(sink).expect("native Opus encoder initializes"),
        )
        .unwrap();
    assert!(gate.wait_for_calls(1));

    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let update_session = Arc::clone(&session);
    let update_worker = thread::spawn(move || {
        let _ = result_tx.send(update_session.set_filter_graph(Some("tremolo=f=5:d=0.6")));
    });
    assert!(wait_for_pending_filter_update(&session));

    session
        .stop()
        .expect("stop cancels output and joins the worker");
    assert!(matches!(
        result_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
        Err(PlaybackError::Cancelled)
    ));
    update_worker.join().expect("filter update worker joins");
    assert_eq!(session.state(), PlaybackState::Stopped);
    assert!(gate.snapshot().cancelled);
}

#[test]
fn filter_replacement_and_seek_are_applied_while_paused() {
    let source = TempWave::constant(48_000, 4096);
    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));

    let pause_session = Arc::clone(&session);
    let (paused, paused_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = paused.send(pause_session.pause());
    });
    assert!(wait_for_state(&session, PlaybackState::Pausing));
    gate.release(1);
    assert!(
        paused_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("pause should reach a frame boundary")
            .is_ok()
    );
    assert_eq!(session.state(), PlaybackState::Paused);
    let paused_media_position = session.position().media.map(|media| media.position);

    session.set_filter_graph(Some("volume=0.5")).unwrap();
    assert_eq!(session.state(), PlaybackState::Paused);
    assert_eq!(
        session.position().media.map(|media| media.position),
        paused_media_position
    );
    let seek_position = Duration::from_millis(500);
    session.seek(seek_position).unwrap();
    assert_eq!(session.position().submitted, seek_position);
    assert_eq!(
        session.position().media.map(|media| media.position),
        Some(seek_position)
    );
    assert_eq!(
        session.position().media.map(|media| media.accuracy),
        Some(MediaPositionAccuracy::Estimated)
    );
    assert_eq!(gate.snapshot().flushes.last(), Some(&seek_position));

    session.resume().unwrap();
    assert!(gate.wait_for_calls(2));
    gate.release(1);
    assert!(gate.wait_for_accepted(2));
    let snapshot = gate.snapshot();
    let (start_sample, first_sample) = snapshot.accepted[1];
    assert!(start_sample >= 24_000);
    let input_level = 4096.0 / 32768.0 / std::f32::consts::SQRT_2;
    assert!((first_sample - input_level * 0.5).abs() < 0.001);
    session.stop().unwrap();
}
