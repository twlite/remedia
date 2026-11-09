use super::*;

#[test]
fn passthrough_to_pcm_can_be_staged_while_paused() {
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(
                include_bytes!("../../../../tests/fixtures/generated-opus-passthrough.ogg")
                    .to_vec(),
            ),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));

    let pause_session = Arc::clone(&session);
    let (pause_tx, pause_rx) = mpsc::sync_channel(1);
    let pause_worker = thread::spawn(move || {
        let _ = pause_tx.send(pause_session.pause());
    });
    assert!(wait_for_state(&session, PlaybackState::Pausing));
    handle.release(1);
    pause_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    pause_worker.join().unwrap();
    assert_eq!(session.state(), PlaybackState::Paused);

    session.set_volume(0.75).unwrap();
    assert!(wait_for_processing_mode(
        &session,
        PlaybackProcessingMode::Transcoding
    ));
    assert_eq!(session.state(), PlaybackState::Paused);

    session.resume().unwrap();
    handle.release(1_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert!(!handle.snapshot().packets.is_empty());
}

#[test]
fn seeking_during_passthrough_uses_pcm_trimming_and_invalidates_old_packets() {
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(
                include_bytes!("../../../../tests/fixtures/generated-opus-passthrough.ogg")
                    .to_vec(),
            ),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));

    let seeking_session = Arc::clone(&session);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let seeker = thread::spawn(move || {
        let _ = response_tx.send(seeking_session.seek(Duration::from_millis(500)));
    });
    assert!(handle.wait_for_pending_seek(&session));
    handle.release(1);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    seeker.join().unwrap();

    assert!(
        handle.wait_for_calls(2),
        "seek falls back to decoded output"
    );
    handle.release(1_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.flushes, 1);
    assert!(
        snapshot
            .packets
            .iter()
            .any(|packet| packet.start_sample >= 24_000)
    );
}

#[test]
fn one_opus_source_switches_between_passthrough_and_live_filtering() {
    let fixture = include_bytes!("../../../../tests/fixtures/generated-opus-passthrough.ogg");
    let (source, producer) = InputSource::byte_stream(1_024).unwrap();
    let source_bytes = fixture.to_vec();
    let producer_thread = thread::spawn(move || {
        for chunk in source_bytes.chunks(317) {
            producer.write(chunk).unwrap();
        }
        producer.finish().unwrap();
    });
    let (sink, handle) = StepPacketSink::new();
    let output = OpusEncoderOutput::new(sink).unwrap();
    let session = Arc::new(PlaybackSession::new());
    session.play(source, output).expect("Opus source starts");

    assert!(handle.wait_for_calls(1));
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    let update_session = Arc::clone(&session);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let first_update = thread::spawn(move || {
        let _ = response_tx.send(update_session.set_filter_graph(Some("bass=gain=8")));
    });
    assert!(handle.wait_for_pending_filter(&session));
    handle.release(1);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    first_update.join().unwrap();
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding),
        "filter update resolves only after PCM mode is active"
    );

    assert!(
        handle.wait_for_calls(2),
        "PCM transcoding reaches the same sink"
    );
    let update_session = Arc::clone(&session);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let second_update = thread::spawn(move || {
        let _ = response_tx.send(update_session.set_filter_graph(Some("atempo=1.05")));
    });
    assert!(handle.wait_for_pending_filter(&session));
    handle.release(1);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    second_update.join().unwrap();

    assert!(handle.wait_for_calls(3), "tempo graph remains active");
    let update_session = Arc::clone(&session);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let third_update = thread::spawn(move || {
        let _ = response_tx.send(update_session.set_filter_graph(Some("tremolo=f=5:d=0.6")));
    });
    assert!(handle.wait_for_pending_filter(&session));
    handle.release(1);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    third_update.join().unwrap();

    assert!(handle.wait_for_calls(4), "replacement graph remains active");
    replace_graph_while_packet_sender_is_blocked(Arc::clone(&session), handle.clone(), None);
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough),
        "clear resolves only after passthrough mode is active"
    );

    assert!(handle.wait_for_calls(5), "clear returns to passthrough");
    let update_session = Arc::clone(&session);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let set_gain = thread::spawn(move || {
        let _ = response_tx.send(update_session.set_volume(0.75));
    });
    assert!(wait_for_volume(&session, 0.75));
    handle.release(1);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    set_gain.join().unwrap();
    assert!(
        handle.wait_for_calls(6),
        "non-unity gain reactivates PCM processing"
    );
    let update_session = Arc::clone(&session);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let restore_gain = thread::spawn(move || {
        let _ = response_tx.send(update_session.set_volume(1.0));
    });
    assert!(wait_for_volume(&session, 1.0));
    handle.release(1);
    assert!(wait_for_processing_mode(
        &session,
        PlaybackProcessingMode::Passthrough
    ));
    assert!(
        handle.wait_for_calls(7),
        "passthrough resumes on the same sink"
    );
    // A finite source may reach natural EOF as soon as the last queued
    // packet is released. The transition is verified by the settled mode,
    // the seventh sink call, and the final passthrough packets below; it
    // must not depend on observing the transient Playing state.
    assert!(
        matches!(
            session.state(),
            PlaybackState::Playing | PlaybackState::Completed
        ),
        "the restored mode must not stop or fail playback"
    );
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough),
        "unity gain returns to passthrough on the same source"
    );
    handle.release(1_000);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    restore_gain.join().unwrap();
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough),
        "unity gain returns to passthrough"
    );
    assert_eq!(session.wait(), PlaybackState::Completed);
    producer_thread.join().unwrap();
    let snapshot = handle.snapshot();
    assert!(snapshot.finished);
    assert_eq!(snapshot.flushes, 0, "mode changes must not seek-flush RTP");
    assert_eq!(session.filter_graph(), None);
    assert_eq!(session.volume(), 1.0);
    assert!(
        snapshot
            .packets
            .iter()
            .any(|packet| packet.start_sample > 0)
    );
    assert!(snapshot.packets.windows(2).all(|pair| {
        pair[0]
            .start_sample
            .saturating_add(u64::from(pair[0].media_samples_per_channel))
            == pair[1].start_sample
    }));

    let mut source_reader = AudioDecoder::open_for_playback(
        InputSource::bytes(fixture.to_vec()),
        DecoderOptions::default(),
    )
    .unwrap();
    let mut source_payloads = Vec::new();
    while let Some(packet) = source_reader.read_compressed_packet().unwrap() {
        source_payloads.push(packet.packet.data().unwrap().to_vec());
    }
    assert_eq!(
        snapshot.packets.last().map(|packet| &packet.bytes),
        source_payloads.last(),
        "the final packet returned to passthrough on the same source"
    );
    assert_eq!(
        snapshot.packets.first().map(|packet| &packet.bytes),
        source_payloads.first()
    );
    assert!(snapshot.packets.iter().any(|packet| {
        source_payloads
            .iter()
            .all(|source_payload| source_payload != &packet.bytes)
    }));
}
