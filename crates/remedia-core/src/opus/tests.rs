use std::sync::{Arc, Mutex};

use super::*;

#[derive(Clone, Default)]
pub(super) struct CaptureSink(pub(super) Arc<Mutex<CaptureState>>);

#[derive(Default)]
pub(super) struct CaptureState {
    pub(super) packets: Vec<EncodedOpusPacket>,
    flushes: usize,
    flush_sample_positions: Vec<u64>,
    pub(super) finished: bool,
    fail: bool,
}

impl OpusPacketSink for CaptureSink {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        let mut state = lock(&self.0);
        if state.fail {
            return Err(OutputError::new("capture sink failure"));
        }
        state.packets.push(packet);
        Ok(())
    }

    fn flush(
        &self,
        _position: Duration,
        _cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        lock(&self.0).flushes += 1;
        Ok(())
    }

    fn flush_samples(
        &self,
        sample_position: u64,
        _cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut state = lock(&self.0);
        state.flushes += 1;
        state.flush_sample_positions.push(sample_position);
        Ok(())
    }

    fn finish(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        lock(&self.0).finished = true;
        Ok(())
    }

    fn cancel(&self) {}
}

pub(super) fn pcm(start_sample: u64, samples_per_channel: usize) -> PcmFrame {
    PcmFrame {
        samples: vec![0.0; samples_per_channel * 2],
        samples_per_channel,
        start_sample,
    }
}

pub(super) fn assert_settings(output: &OpusEncoderOutput, config: OpusEncoderConfig) {
    let mut state = lock(&output.state);
    let encoder = state.encoder.as_mut().expect("PCM initialized encoder");
    assert_eq!(
        encoder.get_bitrate().unwrap(),
        Bitrate::Bits(config.bitrate)
    );
    assert_eq!(encoder.get_complexity().unwrap(), config.complexity);
    assert_eq!(encoder.get_vbr().unwrap(), config.vbr);
    assert_eq!(encoder.get_application().unwrap(), Application::Audio);
    assert_eq!(encoder.get_sample_rate().unwrap(), OUTPUT_SAMPLE_RATE_HZ);
}

#[test]
fn defaults_and_custom_settings_reach_native_controls_and_survive_resets() {
    for config in [
        OpusEncoderConfig::default(),
        OpusEncoderConfig {
            bitrate: 64_000,
            complexity: 3,
            vbr: false,
        },
        OpusEncoderConfig {
            bitrate: 500,
            complexity: 0,
            vbr: true,
        },
        OpusEncoderConfig {
            bitrate: 512_000,
            complexity: 10,
            vbr: false,
        },
    ] {
        let output = OpusEncoderOutput::new_with_config(CaptureSink::default(), config).unwrap();
        assert!(lock(&output.state).encoder.is_none());
        let cancellation = CancellationToken::new();
        output.write_frame(pcm(0, 960), &cancellation).unwrap();
        assert_settings(&output, config);
        output.flush_samples(4_800, &cancellation).unwrap();
        output.write_frame(pcm(4_800, 960), &cancellation).unwrap();
        assert_settings(&output, config);
        output.finish(&cancellation).unwrap();
    }
    let output = OpusEncoderOutput::new(CaptureSink::default()).unwrap();
    output
        .write_frame(pcm(0, 960), &CancellationToken::new())
        .unwrap();
    assert_settings(&output, OpusEncoderConfig::default());
}

#[test]
fn invalid_settings_fail_before_native_initialization() {
    for bitrate in [i32::MIN, -1, 0, 499, 512_001, i32::MAX] {
        let config = OpusEncoderConfig {
            bitrate,
            ..Default::default()
        };
        assert!(OpusEncoderOutput::new_with_config(CaptureSink::default(), config).is_err());
    }
    for complexity in [-1, 11, i32::MAX] {
        let config = OpusEncoderConfig {
            complexity,
            ..Default::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("complexity")
        );
    }
}

#[test]
fn concurrent_outputs_have_independent_native_encoders() {
    let configs = [
        OpusEncoderConfig::default(),
        OpusEncoderConfig {
            bitrate: 96_000,
            complexity: 2,
            vbr: false,
        },
    ];
    let outputs = configs
        .map(|config| OpusEncoderOutput::new_with_config(CaptureSink::default(), config).unwrap());
    std::thread::scope(|scope| {
        for output in &outputs {
            scope.spawn(move || {
                output
                    .write_frame(pcm(0, 960), &CancellationToken::new())
                    .unwrap();
            });
        }
    });
    outputs[0]
        .flush_samples(0, &CancellationToken::new())
        .unwrap();
    for (output, config) in outputs.iter().zip(configs) {
        assert_settings(output, config);
    }
}

#[test]
fn encoded_packet_delivery_keeps_libopus_encoder_uninitialized() {
    let sink = CaptureSink::default();
    let state = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new_with_config(
        sink,
        OpusEncoderConfig {
            bitrate: 64_000,
            complexity: 1,
            vbr: false,
        },
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    assert!(lock(&output.state).encoder.is_none());

    EncodedOpusOutput::write_encoded_packet(
        &output,
        EncodedOpusPacket {
            bytes: vec![0xF8, 0xFF, 0xFE],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        },
        &cancellation,
    )
    .unwrap();

    assert!(lock(&output.state).encoder.is_none());
    let packets = &lock(&state).packets;
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].bytes, [0xF8, 0xFF, 0xFE]);
}

#[test]
fn encodes_complete_stereo_frames_and_keeps_timeline_metadata() {
    let sink = CaptureSink::default();
    let state = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new(sink).unwrap();
    let cancellation = CancellationToken::new();

    output.write_frame(pcm(0, 400), &cancellation).unwrap();
    assert!(lock(&state).packets.is_empty());
    output.write_frame(pcm(400, 560), &cancellation).unwrap();
    let packets = &lock(&state).packets;
    assert_eq!(packets.len(), 1);
    assert!(!packets[0].bytes.is_empty());
    assert_eq!(packets[0].samples_per_channel, 960);
    assert_eq!(packets[0].media_samples_per_channel, 960);
    assert_eq!(packets[0].start_sample, 0);
}

#[test]
fn finish_pads_one_partial_frame_and_reports_both_durations() {
    let sink = CaptureSink::default();
    let state = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new(sink).unwrap();
    let cancellation = CancellationToken::new();

    output.write_frame(pcm(120, 240), &cancellation).unwrap();
    output.finish(&cancellation).unwrap();
    let state = lock(&state);
    assert!(state.finished);
    assert_eq!(state.packets.len(), 1);
    assert_eq!(state.packets[0].samples_per_channel, 960);
    assert_eq!(state.packets[0].media_samples_per_channel, 240);
    assert_eq!(state.packets[0].start_sample, 120);
}

#[test]
fn sink_errors_propagate_from_packet_submission() {
    let sink = CaptureSink::default();
    lock(&sink.0).fail = true;
    let output = OpusEncoderOutput::new(sink).unwrap();
    let error = output
        .write_frame(pcm(0, 960), &CancellationToken::new())
        .unwrap_err();
    assert!(error.to_string().contains("capture sink failure"));
}

#[test]
fn flush_discards_partial_pcm_and_restarts_encoder_at_seek_position() {
    let sink = CaptureSink::default();
    let state = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new(sink).unwrap();
    let cancellation = CancellationToken::new();

    output.write_frame(pcm(0, 240), &cancellation).unwrap();
    output
        .flush(Duration::from_millis(100), &cancellation)
        .unwrap();
    output.write_frame(pcm(4_800, 960), &cancellation).unwrap();
    let packets = &lock(&state).packets;
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].start_sample, 4_800);
}

#[test]
fn sample_accurate_flush_preserves_non_round_duration_boundaries() {
    for sample_position in [1_024, 1_337, 2_003, 4_096, 10_001] {
        let sink = CaptureSink::default();
        let state = Arc::clone(&sink.0);
        let output = OpusEncoderOutput::new(sink).unwrap();
        let cancellation = CancellationToken::new();
        output
            .write_frame(pcm(0, sample_position as usize), &cancellation)
            .unwrap();
        output
            .flush_samples(sample_position, &cancellation)
            .unwrap();
        assert!(
            output
                .write_frame(pcm(sample_position, 960), &cancellation)
                .is_ok()
        );
        assert_eq!(
            lock(&state).flush_sample_positions,
            [sample_position],
            "the exact sample position reaches the packet sink"
        );
    }
}

#[test]
fn live_filter_transition_keeps_partial_opus_pcm_and_does_not_flush_sink() {
    let sink = CaptureSink::default();
    let state = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new(sink).unwrap();
    let cancellation = CancellationToken::new();

    output.write_frame(pcm(0, 1_024), &cancellation).unwrap();
    output.filter_transition(1_024, &cancellation).unwrap();
    output
        .write_frame(pcm(1_024, 1_024), &cancellation)
        .unwrap();
    output.finish(&cancellation).unwrap();

    let state = lock(&state);
    assert_eq!(state.packets.len(), 3);
    assert_eq!(state.packets[0].start_sample, 0);
    assert_eq!(state.packets[1].start_sample, 960);
    assert_eq!(state.packets[2].start_sample, 1_920);
    assert_eq!(state.packets[1].media_samples_per_channel, 960);
    assert_eq!(state.packets[2].media_samples_per_channel, 128);
    assert_eq!(state.flushes, 0, "filter changes must not flush the sink");
    assert!(state.finished);
}

#[test]
fn queued_packets_own_only_the_encoded_payload() {
    let sink = CaptureSink::default();
    let captured = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new(sink).unwrap();
    output
        .write_frame(pcm(0, 960 * 4), &CancellationToken::new())
        .unwrap();
    let state = lock(&captured);
    assert_eq!(state.packets.len(), 4);
    for (index, packet) in state.packets.iter().enumerate() {
        assert_eq!(packet.bytes.capacity(), packet.bytes.len());
        assert_eq!(packet.start_sample, index as u64 * 960);
    }
}

#[test]
fn aligned_frames_put_the_first_frame_on_the_source_grid_and_report_true_media() {
    let sink = CaptureSink::default();
    let output = OpusEncoderOutput::new(sink.clone()).unwrap();
    let cancellation = CancellationToken::new();
    let encoded: &dyn EncodedOpusOutput = &output;

    // 24 300 = 25 * 960 + 300: 660 samples remain in the packet containing the target.
    output.flush_samples(24_300, &cancellation).unwrap();
    encoded.align_encoder_frames(24_300).unwrap();
    output.write_frame(pcm(24_300, 660), &cancellation).unwrap();
    {
        let state = lock(&sink.0);
        assert_eq!(state.flush_sample_positions, [24_300]);
        assert_eq!(state.packets.len(), 1);
        let first = &state.packets[0];
        assert_eq!(
            first.start_sample, 24_300,
            "the packet starts at the first real sample"
        );
        assert_eq!(
            first.samples_per_channel, 960,
            "but occupies a whole 20 ms frame"
        );
        assert_eq!(
            first.media_samples_per_channel, 660,
            "of which 300 samples are silence"
        );
    }

    // The encoder is now on the grid, so switching back needs no padded partial frame.
    encoded.prepare_passthrough(24_960, &cancellation).unwrap();
    assert_eq!(lock(&sink.0).packets.len(), 1);
    encoded
        .write_encoded_packet(
            EncodedOpusPacket {
                bytes: vec![0xF8, 0xFF, 0xFE],
                samples_per_channel: 960,
                media_samples_per_channel: 960,
                start_sample: 24_960,
            },
            &cancellation,
        )
        .unwrap();
    output.finish(&cancellation).unwrap();
    assert_eq!(lock(&sink.0).packets.len(), 2);
}

#[test]
fn encoder_lookahead_constant_matches_libopus() {
    // Seamless packet recovery depends on this being exactly the encoder's algorithmic delay.
    let output = OpusEncoderOutput::new(CaptureSink::default()).unwrap();
    output
        .write_frame(pcm(0, 960), &CancellationToken::new())
        .unwrap();
    let lookahead = lock(&output.state)
        .encoder
        .as_mut()
        .unwrap()
        .get_lookahead()
        .unwrap();
    assert_eq!(lookahead as u32, crate::decoder::OPUS_ENCODER_LOOKAHEAD);
}

#[test]
fn aligning_without_audio_never_emits_or_requires_an_encoder() {
    let sink = CaptureSink::default();
    let output = OpusEncoderOutput::new(sink.clone()).unwrap();
    let cancellation = CancellationToken::new();
    let encoded: &dyn EncodedOpusOutput = &output;

    // A seek to the very end can finish before any PCM follows. The alignment silence is dropped.
    output.flush_samples(1_234, &cancellation).unwrap();
    encoded.align_encoder_frames(1_234).unwrap();
    output.finish(&cancellation).unwrap();

    let state = lock(&sink.0);
    assert!(state.packets.is_empty());
    assert!(state.finished);
}

#[test]
fn aligning_on_the_grid_or_mid_frame_changes_nothing() {
    let sink = CaptureSink::default();
    let output = OpusEncoderOutput::new(sink.clone()).unwrap();
    let cancellation = CancellationToken::new();
    let encoded: &dyn EncodedOpusOutput = &output;

    encoded.align_encoder_frames(9_600).unwrap();
    output.write_frame(pcm(9_600, 960), &cancellation).unwrap();
    // A partial frame is already pending, so a late request is ignored rather than corrupting it.
    output.write_frame(pcm(10_560, 100), &cancellation).unwrap();
    encoded.align_encoder_frames(10_660).unwrap();
    output.write_frame(pcm(10_660, 860), &cancellation).unwrap();

    let state = lock(&sink.0);
    assert_eq!(state.packets.len(), 2);
    assert_eq!(state.packets[0].start_sample, 9_600);
    assert_eq!(state.packets[1].start_sample, 10_560);
    assert_eq!(state.packets[1].media_samples_per_channel, 960);
}
