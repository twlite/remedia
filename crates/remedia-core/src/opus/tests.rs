use std::sync::{Arc, Mutex};

use super::*;

#[derive(Clone, Default)]
struct CaptureSink(Arc<Mutex<CaptureState>>);

#[derive(Default)]
struct CaptureState {
    packets: Vec<EncodedOpusPacket>,
    flushes: usize,
    flush_sample_positions: Vec<u64>,
    finished: bool,
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

fn pcm(start_sample: u64, samples_per_channel: usize) -> PcmFrame {
    PcmFrame {
        samples: vec![0.0; samples_per_channel * 2],
        samples_per_channel,
        start_sample,
    }
}

fn assert_settings(output: &OpusEncoderOutput, config: OpusEncoderConfig) {
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
