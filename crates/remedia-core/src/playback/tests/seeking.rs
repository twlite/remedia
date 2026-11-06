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

#[test]
fn forward_seek_in_byte_stream_discards_audio_without_storing_the_skipped_input() {
    let source = TempWave::from_samples(
        (0..144_000)
            .map(|index| if index < 48_000 { 1200 } else { 12_000 })
            .collect(),
    );
    let bytes = fs::read(&source.path).unwrap();
    let (input, writer) = InputSource::byte_stream(4 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in bytes.chunks(257) {
            if writer.write(chunk).is_err() {
                return;
            }
            assert!(writer.pending_bytes() <= writer.capacity_bytes());
        }
        writer.finish().unwrap();
    });

    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(input, output).unwrap();
    pause_at_first_blocked_frame(&session, &gate);

    let target = Duration::from_millis(1_150);
    session.seek(target).unwrap();
    assert_eq!(session.state(), PlaybackState::Paused);
    assert_eq!(session.position().submitted, target);
    assert_eq!(gate.snapshot().flushes, vec![target]);
    let before_resume = gate.snapshot().accepted.len();

    session.resume().unwrap();
    assert!(gate.wait_for_calls(before_resume + 1));
    gate.release(1);
    assert!(gate.wait_for_accepted(before_resume + 1));
    let snapshot = gate.snapshot();
    let (start, first_sample) = snapshot.accepted[before_resume];
    assert!(start >= 55_200, "pre-seek PCM must not reach output");
    assert!(
        first_sample > 0.2,
        "the retained frame must come from the target segment"
    );

    session.stop().unwrap();
    feeder.join().unwrap();
}

#[test]
fn backward_seek_on_byte_stream_is_rejected_without_consuming_pending_frames() {
    let source = TempWave::constant(144_000, 4096);
    let bytes = fs::read(&source.path).unwrap();
    let (input, writer) = InputSource::byte_stream(8 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in bytes.chunks(512) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        writer.finish().unwrap();
    });

    let session = Arc::new(PlaybackSession::new());
    let (output, gate) = GateOutput::new();
    session.play(input, output).unwrap();
    pause_at_first_blocked_frame(&session, &gate);

    // Even a small rewind cannot be reconstructed after the producer bytes
    // have been consumed. A rejected seek must not flush or fail playback.
    assert!(matches!(
        session.seek(Duration::ZERO),
        Err(PlaybackError::UnsupportedSeeking(_))
    ));
    assert_eq!(session.state(), PlaybackState::Paused);
    assert!(gate.snapshot().flushes.is_empty());

    session.seek(Duration::from_secs(1)).unwrap();
    assert_eq!(session.position().submitted, Duration::from_secs(1));
    assert!(matches!(
        session.seek(Duration::from_millis(500)),
        Err(PlaybackError::UnsupportedSeeking(_))
    ));
    assert_eq!(session.state(), PlaybackState::Paused);
    assert_eq!(gate.snapshot().flushes, vec![Duration::from_secs(1)]);

    session.stop().unwrap();
    feeder.join().unwrap();
}

#[test]
fn forward_seek_uses_media_timestamps_for_compressed_mp3_byte_streams() {
    let bytes = include_bytes!("../../../tests/fixtures/generated-tone.mp3").to_vec();
    let (input, writer) = InputSource::byte_stream(2 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in bytes.chunks(173) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        writer.finish().unwrap();
    });

    let mut decoder = AudioDecoder::open(input).unwrap();
    let first = decoder.next_frame().unwrap().unwrap();
    assert!(first.start_sample < 5_760);

    decoder.seek(Duration::from_millis(120)).unwrap();
    let next = decoder.next_frame().unwrap().unwrap();
    assert!(next.start_sample >= 5_760);
    assert!(matches!(
        decoder.seek(Duration::from_millis(50)),
        Err(crate::error::DecoderError::UnsupportedSeeking(_))
    ));

    drop(decoder);
    feeder.join().unwrap();
}
