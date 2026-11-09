use super::*;

#[test]
fn opted_in_crossfade_promotes_staged_decoder_without_restarting_output() {
    use super::super::{CrossfadeConfig, CrossfadeEvent, StageIdentity, StageState};

    let outgoing = TempWave::constant(480_000, 8_000);
    let incoming = TempWave::constant(480_000, -8_000);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
        .expect("crossfade options accepted while idle");
    let (output, gate) = GateOutput::new();
    let initial = session
        .play(outgoing.source(), output)
        .expect("outgoing source opened");
    assert_eq!(initial.duration, Some(Duration::from_secs(10)));
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    assert!(gate.wait_for_calls(1));

    let identity = StageIdentity {
        expected_generation: session.source_generation(),
        entry_id: "entry-b".to_string(),
        queue_revision: 1,
        source_revision: 0,
    };
    let first = session
        .stage_next(incoming.source(), identity.clone())
        .expect("stage source accepted");
    assert!(matches!(
        first.state,
        StageState::Preparing | StageState::Ready
    ));
    let duplicate = session
        .stage_next(incoming.source(), identity)
        .expect("duplicate stage is idempotent");
    assert!(matches!(
        duplicate.state,
        StageState::Preparing | StageState::Ready
    ));

    let deadline = Instant::now() + TEST_TIMEOUT;
    while !matches!(
        session.staged_source().map(|stage| stage.state),
        Some(StageState::Ready)
    ) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        session.staged_source().map(|stage| stage.state),
        Some(StageState::Ready),
        "the next source should decode into its bounded buffer",
    );

    let prepared = lock(&session.stage).as_ref().cloned().unwrap();
    gate.release(250);
    advance_buffered_fade(&gate, &prepared, 251, 500);
    gate.release(1_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 2);
    let frames = gate.snapshot();
    assert!(frames.finished);
    // 10 seconds outgoing + 10 seconds incoming - 5 seconds overlapped.
    let frame_count = frames.accepted.len();
    assert!(
        (740..=770).contains(&frame_count),
        "crossfading should produce approximately 15 seconds of 20ms frames, got {frame_count}",
    );
    assert!(
        frames
            .accepted
            .windows(2)
            .all(|frames| { frames[0].0 + 960 == frames[1].0 })
    );

    let events = session.take_crossfade_events();
    assert!(events.iter().any(|event| matches!(
        event,
        CrossfadeEvent::PrepareRequested { generation: 1, .. }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CrossfadeEvent::TransitionStarted {
            outgoing_generation: 1,
            incoming_generation: 2,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CrossfadeEvent::TransitionCompleted {
            outgoing_generation: 1,
            incoming_generation: 2,
            ..
        }
    )));
}

#[test]
fn normal_playback_cannot_stage_without_crossfade_opt_in() {
    use super::super::StageIdentity;
    let source = TempWave::constant(48_000, 4_096);
    let session = PlaybackSession::new();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    assert!(matches!(
        session.stage_next(
            source.source(),
            StageIdentity {
                expected_generation: session.source_generation(),
                entry_id: "next".to_string(),
                queue_revision: 0,
                source_revision: 0,
            },
        ),
        Err(PlaybackError::Cancelled)
    ));
    session.stop().unwrap();
}

#[test]
fn crossfade_supports_consecutive_promotions_and_rejects_stale_revisions() {
    use super::super::{CrossfadeConfig, CrossfadeEvent, StageIdentity, StageState};
    let source = TempWave::constant(480_000, 4_000);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
        .unwrap();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    for (generation, entry_id) in [(1, "second"), (2, "third")] {
        let identity = StageIdentity {
            expected_generation: generation,
            entry_id: entry_id.into(),
            queue_revision: generation,
            source_revision: 1,
        };
        session
            .stage_next(source.source(), identity.clone())
            .unwrap();
        let deadline = Instant::now() + TEST_TIMEOUT;
        while session
            .staged_source()
            .is_none_or(|s| s.state != StageState::Ready)
        {
            assert!(Instant::now() < deadline, "staging deadline");
            thread::sleep(Duration::from_millis(1));
        }
        let mut stale = identity.clone();
        stale.source_revision = 0;
        assert!(session.stage_next(source.source(), stale).is_err());
        assert_eq!(session.staged_source().unwrap().identity, identity);
        let prepared = lock(&session.stage).as_ref().cloned().unwrap();
        if generation == 1 {
            gate.release(250);
            advance_buffered_fade(&gate, &prepared, 251, 500);
            assert_eq!(session.source_generation(), 2);
        } else {
            gate.release(1);
            advance_buffered_fade(&gate, &prepared, 501, 750);
        }
    }
    gate.release(2_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 3);
    let frames = gate.snapshot();
    assert_eq!(frames.accepted.len(), 1_000);
    assert!(frames.accepted.windows(2).all(|w| w[0].0 + 960 == w[1].0));
    let events = session.take_crossfade_events();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, CrossfadeEvent::TransitionCompleted { .. }))
            .count(),
        2
    );
    session.stop().unwrap();
}

#[test]
fn cancelled_stage_cannot_be_replaced_by_an_older_revision() {
    use super::super::{CrossfadeConfig, StageIdentity};
    let source = TempWave::constant(480_000, 4_000);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
        .unwrap();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    let mut identity = StageIdentity {
        expected_generation: 1,
        entry_id: "next".into(),
        queue_revision: 2,
        source_revision: 1,
    };
    session
        .stage_next(source.source(), identity.clone())
        .unwrap();
    session.clear_staged_source();
    identity.queue_revision = 1;
    assert!(session.stage_next(source.source(), identity).is_err());
    assert!(session.staged_source().is_none());
    session.stop().unwrap();
}

#[test]
fn unavailable_preparation_never_stalls_or_promotes() {
    use super::super::{CrossfadeConfig, CrossfadeEvent, StageIdentity};
    for mode in ["ignored", "failed", "expired", "cancelled"] {
        let source = TempWave::constant(480_000, 4_000);
        let session = PlaybackSession::new();
        session
            .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
            .unwrap();
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));
        let identity = StageIdentity {
            expected_generation: 1,
            entry_id: mode.into(),
            queue_revision: 1,
            source_revision: 0,
        };
        match mode {
            "failed" => {
                session
                    .stage_next(
                        InputSource::file("/nonexistent-remedia-crossfade.wav"),
                        identity,
                    )
                    .unwrap();
            }
            "expired" => {
                session
                    .stage_next_with_expiration(source.source(), identity, Some(1))
                    .unwrap();
            }
            "cancelled" => {
                session.stage_next(source.source(), identity).unwrap();
                session.clear_staged_source();
            }
            _ => {}
        }
        gate.release(usize::MAX);
        assert_eq!(session.wait(), PlaybackState::Completed, "{mode}");
        assert_eq!(session.source_generation(), 1, "{mode}");
        assert_eq!(gate.snapshot().accepted.len(), 500, "{mode}");
        assert!(
            !session
                .take_crossfade_events()
                .iter()
                .any(|event| matches!(event, CrossfadeEvent::TransitionStarted { .. })),
            "{mode}"
        );
        assert!(session.staged_source().is_none());
        session.stop().unwrap();
    }
}

#[test]
fn crossfade_pause_processing_seek_and_stop_preserve_control_semantics() {
    use super::super::{CrossfadeConfig, StageIdentity};
    let source = TempWave::constant(480_000, 4_000);
    let session = Arc::new(PlaybackSession::new());
    session
        .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
        .unwrap();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    pause_at_first_blocked_frame(&session, &gate);
    session
        .stage_next(
            source.source(),
            StageIdentity {
                expected_generation: 1,
                entry_id: "next".into(),
                queue_revision: 1,
                source_revision: 0,
            },
        )
        .unwrap();
    session.set_filter_graph(Some("anull")).unwrap();
    session.set_volume(0.5).unwrap();
    assert_eq!(session.state(), PlaybackState::Paused);
    assert!(session.staged_source().is_some());
    session.seek(Duration::from_secs(2)).unwrap();
    assert!(session.staged_source().is_none());
    assert_eq!(session.source_generation(), 2);
    assert!(
        session
            .stage_next(
                source.source(),
                StageIdentity {
                    expected_generation: 1,
                    entry_id: "stale".into(),
                    queue_revision: 2,
                    source_revision: 0
                }
            )
            .is_err()
    );
    assert_eq!(session.state(), PlaybackState::Paused);
    assert_eq!(session.volume(), 0.5);
    session.resume().unwrap();
    assert!(gate.wait_for_calls(2));
    session.stop().unwrap();
    assert_eq!(session.state(), PlaybackState::Stopped);
}

#[test]
fn preparation_clock_uses_fade_plus_lead_and_late_staging_does_not_delay_eof() {
    use super::super::{CrossfadeConfig, CrossfadeEvent, StageIdentity};
    let source = TempWave::constant(1_920_000, 4_000);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
        .unwrap();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    assert!(session.take_crossfade_events().is_empty());
    gate.release(250);
    assert!(gate.wait_for_calls(251));
    assert!(session.take_crossfade_events().iter().any(|event| matches!(
        event,
        CrossfadeEvent::PrepareRequested {
            generation: 1,
            remaining_ms: 35_000
        }
    )));
    gate.release(1_501);
    assert!(gate.wait_for_calls(1_752));
    session
        .stage_next(
            source.source(),
            StageIdentity {
                expected_generation: 1,
                entry_id: "too-late".into(),
                queue_revision: 1,
                source_revision: 0,
            },
        )
        .unwrap();
    gate.release(usize::MAX);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 1);
    assert_eq!(gate.snapshot().accepted.len(), 2_000);
    session.stop().unwrap();
}

/// Model an output clock without assuming that an unpaced release-build mixer
/// yields to the independently scheduled decoder. Keep the last fade frame
/// blocked so callers can stage the next occurrence before its own deadline.
fn advance_buffered_fade(
    gate: &GateHandle,
    stage: &Arc<super::super::PreparedStage>,
    first_call: usize,
    last_call: usize,
) {
    assert!(gate.wait_for_calls(first_call));
    for call in first_call..last_call {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while stage.buffer.buffered_samples() < 960 {
            assert!(
                Instant::now() < deadline,
                "decoder did not refill during fade at frame {call}"
            );
            assert!(stage.snapshot().error.is_none(), "staged decoder failed");
            thread::yield_now();
        }
        gate.release(1);
        assert!(gate.wait_for_calls(call + 1));
    }
}

#[test]
fn interrupted_fade_continues_outgoing_without_waiting_for_incoming_pcm() {
    use super::super::{CrossfadeConfig, CrossfadeEvent, StageIdentity, StageState};
    let source = TempWave::constant(480_000, 4_000);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(5_000, 30_000).unwrap()))
        .unwrap();
    let (output, gate) = GateOutput::new();
    session.play(source.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    session
        .stage_next(
            source.source(),
            StageIdentity {
                expected_generation: 1,
                entry_id: "interrupted".into(),
                queue_revision: 1,
                source_revision: 0,
            },
        )
        .unwrap();
    let stage = lock(&session.stage).as_ref().cloned().unwrap();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while stage.snapshot().state != StageState::Ready {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    gate.release(250);
    assert!(gate.wait_for_calls(251));
    assert_eq!(session.source_generation(), 2);
    // Simulate the provider losing its PCM while the first mixed frame is in
    // the output. The next read must cancel overlap rather than wait on it.
    stage.buffer.cancel();
    gate.release(1_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 1);
    let frames = gate.snapshot();
    assert_eq!(frames.accepted.len(), 500);
    assert!(frames.finished);
    assert!(frames.accepted.windows(2).all(|w| w[0].0 + 960 == w[1].0));
    let events = session.take_crossfade_events();
    assert!(events.iter().any(|event| matches!(
        event,
        CrossfadeEvent::TransitionCancelled {
            outgoing_generation: 1,
            incoming_generation: 2,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, CrossfadeEvent::TransitionCompleted { .. }))
    );
    session.stop().unwrap();
}

#[test]
fn crossfade_without_a_ready_stage_preserves_all_source_opus_packets() {
    use super::super::CrossfadeConfig;
    let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
    let (sink, handle) = StepPacketSink::new();
    handle.release(1000);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(1000, 5000).unwrap()))
        .unwrap();
    session
        .play(
            InputSource::bytes(fixture.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    let actual = handle.snapshot().packets;
    let (sink, handle) = StepPacketSink::new();
    handle.release(1000);
    let reference = PlaybackSession::new();
    reference
        .play(
            InputSource::bytes(fixture.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert_eq!(reference.wait(), PlaybackState::Completed);
    assert_eq!(actual, handle.snapshot().packets);
}

#[test]
fn opus_crossfade_enters_pcm_at_overlap_and_keeps_exact_output_sample_accounting() {
    use super::super::{CrossfadeConfig, StageIdentity};
    let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
    let (sink, handle) = StepPacketSink::new();
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(1000, 5000).unwrap()))
        .unwrap();
    session
        .play(
            InputSource::bytes(fixture.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    session
        .stage_next(
            InputSource::bytes(fixture.to_vec()),
            StageIdentity {
                expected_generation: session.source_generation(),
                entry_id: "next".into(),
                queue_revision: 1,
                source_revision: 1,
            },
        )
        .unwrap();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while session.staged_source().unwrap().buffered_samples < 96000 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    handle.release(1000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 2);
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    let packets = handle.snapshot().packets;
    assert!(packets.windows(2).all(|pair| pair[1].start_sample
        == pair[0].start_sample + u64::from(pair[0].media_samples_per_channel)));
    assert_eq!(
        packets
            .iter()
            .map(|packet| u64::from(packet.media_samples_per_channel))
            .sum::<u64>(),
        145920
    );
}

#[test]
fn crossfade_policy_restores_direct_opus_after_temporary_volume_processing() {
    use super::super::CrossfadeConfig;
    let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .configure_crossfade(Some(CrossfadeConfig::new(1000, 5000).unwrap()))
        .unwrap();
    session
        .play(
            InputSource::bytes(fixture.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    let update_session = Arc::clone(&session);
    let update = thread::spawn(move || update_session.set_volume(0.5));
    let deadline = Instant::now() + TEST_TIMEOUT;
    while session.volume() != 0.5 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    handle.release(1);
    assert!(handle.wait_for_calls(2));
    update.join().unwrap().unwrap();
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    session.set_volume(1.0).unwrap();
    handle.release(1);
    assert!(handle.wait_for_calls(3));
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );
    handle.release(1000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    let packets = handle.snapshot().packets;
    assert_eq!(
        packets
            .iter()
            .map(|packet| u64::from(packet.media_samples_per_channel))
            .sum::<u64>(),
        96960
    );
}

#[test]
fn cancelling_http_preparation_preserves_unrelated_playback_and_joins_every_worker() {
    use super::super::{CrossfadeConfig, StageIdentity};
    use std::io::Read;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (seen, request_seen) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        connection.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
        let mut request = [0; 4096];
        assert!(connection.read(&mut request).unwrap() > 0);
        seen.send(()).unwrap();
        // Never send headers. FFmpeg must close this connection on cancellation.
        let _ = connection.read(&mut request);
    });
    let fixture = TempWave::constant(480000, 4096);
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(CrossfadeConfig::new(1000, 5000).unwrap()))
        .unwrap();
    let (output, gate) = GateOutput::new();
    session.play(fixture.source(), output).unwrap();
    assert!(gate.wait_for_calls(1));
    session
        .stage_next(
            InputSource::http_url(format!("http://{address}/stalled")),
            StageIdentity {
                expected_generation: session.source_generation(),
                entry_id: "stalled".into(),
                queue_revision: 1,
                source_revision: 1,
            },
        )
        .unwrap();
    request_seen.recv_timeout(TEST_TIMEOUT).unwrap();
    let unrelated = PlaybackSession::new();
    unrelated.play(fixture.source(), NullOutput::new()).unwrap();
    assert_eq!(unrelated.wait(), PlaybackState::Completed);
    let started = Instant::now();
    session.stop().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(session.staged_source().is_none());
    assert!(lock(&session.worker).is_none());
    assert!(gate.snapshot().cancelled);
    server.join().unwrap();
}
