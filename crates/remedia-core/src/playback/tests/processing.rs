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

mod filters;

mod transitions;
