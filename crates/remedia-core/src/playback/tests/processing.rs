use super::*;

#[test]
fn compatible_opus_source_is_forwarded_without_transcoding() {
    let bytes = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg").to_vec();
    let source = InputSource::bytes(bytes.clone());
    let mut packet_reader =
        AudioDecoder::open_for_playback(source.clone(), DecoderOptions::default()).unwrap();
    assert_eq!(packet_reader.opus_passthrough_rejection(), None);
    let mut expected_payloads = Vec::new();
    while let Some(packet) = packet_reader.read_compressed_packet().unwrap() {
        expected_payloads.push(packet.packet.data().unwrap().to_vec());
    }
    let preroll = packet_reader.packet_history_before(u64::MAX);
    assert_eq!(preroll.len(), 6, "packet history is capped at 120 ms");

    let (sink, handle) = StepPacketSink::new();
    handle.release(1_000);
    let output = OpusEncoderOutput::new(sink).unwrap();
    let session = PlaybackSession::new();
    let info = session.play(source, output).unwrap();
    assert_eq!(info.codec, "opus");
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    assert_eq!(session.wait(), PlaybackState::Completed);

    let packets = handle.snapshot().packets;
    assert_eq!(packets.len(), expected_payloads.len());
    assert_eq!(
        packets
            .iter()
            .map(|packet| packet.bytes.clone())
            .collect::<Vec<_>>(),
        expected_payloads,
        "passthrough must preserve each demuxed Opus payload"
    );
    assert!(
        packets
            .iter()
            .all(|packet| packet.samples_per_channel == 960)
    );
    assert!(packets.iter().enumerate().all(|(index, packet)| {
        packet.start_sample == index as u64 * 960
            && packet.media_samples_per_channel == packet.samples_per_channel
    }));
    assert_eq!(session.position().submitted, Duration::from_millis(2_020));
    assert_eq!(handle.snapshot().flushes, 0);
}

#[test]
fn non_opus_stream_falls_back_to_pcm_and_native_opus_encoding() {
    let source_bytes = include_bytes!("../../../tests/fixtures/generated-tone.mp3").to_vec();
    let reader = AudioDecoder::open_for_playback(
        InputSource::bytes(source_bytes.clone()),
        DecoderOptions::default(),
    )
    .unwrap();
    assert_eq!(
        reader.opus_passthrough_rejection(),
        Some("selected codec is not Opus")
    );

    let (sink, handle) = StepPacketSink::new();
    handle.release(1_000);
    let session = PlaybackSession::new();
    session
        .play(
            InputSource::bytes(source_bytes),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    let packets = handle.snapshot().packets;
    assert!(!packets.is_empty());
    assert!(
        packets
            .iter()
            .all(|packet| packet.samples_per_channel == 960)
    );
    assert!(packets.windows(2).all(|pair| {
        pair[0]
            .start_sample
            .saturating_add(u64::from(pair[0].media_samples_per_channel))
            == pair[1].start_sample
    }));
}

#[test]
fn opus_with_encoder_preskip_falls_back_instead_of_forwarding_trimmed_packets() {
    let source_bytes =
        include_bytes!("../../../tests/fixtures/generated-opus-trimmed.ogg").to_vec();
    let reader = AudioDecoder::open_for_playback(
        InputSource::bytes(source_bytes.clone()),
        DecoderOptions::default(),
    )
    .unwrap();
    assert!(reader.opus_passthrough_rejection().is_some());

    let (sink, handle) = StepPacketSink::new();
    handle.release(1_000);
    let session = PlaybackSession::new();
    session
        .play(
            InputSource::bytes(source_bytes),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert!(!handle.snapshot().packets.is_empty());
}

#[test]
fn output_packet_policy_can_reject_passthrough_and_fall_back_to_pcm() {
    let (sink, handle) = StepPacketSink::without_passthrough();
    handle.release(1_000);
    let session = PlaybackSession::new();
    session
        .play(
            InputSource::bytes(
                include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
            ),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();

    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    assert!(!handle.snapshot().packets.is_empty());
}

#[test]
fn concurrent_opus_sessions_keep_processing_modes_independent() {
    let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
    let (passthrough_sink, passthrough_handle) = StepPacketSink::new();
    let (processing_sink, processing_handle) = StepPacketSink::new();
    passthrough_handle.release(1_000);
    processing_handle.release(1_000);
    let passthrough = PlaybackSession::new();
    let processing = PlaybackSession::new();
    processing.set_volume(0.75).unwrap();

    passthrough
        .play(
            InputSource::bytes(fixture.to_vec()),
            OpusEncoderOutput::new_with_config(
                passthrough_sink,
                crate::OpusEncoderConfig {
                    bitrate: 64_000,
                    complexity: 1,
                    vbr: false,
                },
            )
            .unwrap(),
        )
        .unwrap();
    processing
        .play(
            InputSource::bytes(fixture.to_vec()),
            OpusEncoderOutput::new_with_config(
                processing_sink,
                crate::OpusEncoderConfig {
                    bitrate: 96_000,
                    complexity: 5,
                    vbr: true,
                },
            )
            .unwrap(),
        )
        .unwrap();

    assert_eq!(
        passthrough.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    assert_eq!(
        processing.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    assert_eq!(passthrough.wait(), PlaybackState::Completed);
    assert_eq!(processing.wait(), PlaybackState::Completed);
    assert_eq!(
        passthrough.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    assert_eq!(
        processing.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    assert!(!passthrough_handle.snapshot().packets.is_empty());
    assert!(!processing_handle.snapshot().packets.is_empty());
}

#[test]
fn passthrough_to_pcm_can_be_staged_while_paused() {
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(
                include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
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
                include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
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
    let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
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
                include_bytes!("../../../tests/fixtures/generated-tone.mp3").to_vec(),
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
