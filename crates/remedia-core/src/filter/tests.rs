use super::{FilterGraphError, PcmFilterGraph, PcmProcessor};
use crate::decoder::PcmFrame;

fn pcm(start_sample: u64, samples_per_channel: usize, value: f32) -> PcmFrame {
    PcmFrame {
        samples: vec![value; samples_per_channel * 2],
        samples_per_channel,
        start_sample,
    }
}

fn process(expression: &str, input: PcmFrame, gain: f32) -> Vec<PcmFrame> {
    let mut processor = PcmProcessor::new(Some(expression), input.start_sample).unwrap();
    let source_start_sample = input.start_sample;
    let mut output = Vec::new();
    processor
        .write_frame(input, Some(source_start_sample), false, gain, |frame, _| {
            output.push(frame);
            Ok(())
        })
        .unwrap();
    processor
        .finish(gain, |frame, _| {
            output.push(frame);
            Ok(())
        })
        .unwrap();
    output
}

#[test]
fn rejects_empty_invalid_and_nul_graphs() {
    assert!(matches!(
        PcmProcessor::new(Some(" \t\n"), 0),
        Err(FilterGraphError::Empty)
    ));
    assert!(matches!(
        PcmProcessor::new(Some("filter_that_does_not_exist=1"), 0),
        Err(FilterGraphError::Invalid(_))
    ));
    assert!(matches!(
        PcmProcessor::new(Some("anull\0volume=2"), 0),
        Err(FilterGraphError::NulByte)
    ));
    assert!(matches!(
        PcmProcessor::new(Some("aevalsrc=0[s];[in][s]amix=inputs=2[out]"), 0),
        Err(FilterGraphError::Invalid(message)) if message.contains("separate audio source")
    ));
}

#[test]
fn bot_preset_graph_expressions_are_valid_native_filtergraphs() {
    for expression in [
        "apulsator=mode=sine:hz=0.125:amount=0.7",
        "volume=0.75,bass=gain=8:frequency=110",
        "asetrate=60000,aresample=48000",
        "asetrate=38400,aresample=48000",
        "tremolo=f=5:d=0.6",
        "vibrato=f=5:d=0.5",
    ] {
        PcmProcessor::new(Some(expression), 0)
            .unwrap_or_else(|error| panic!("invalid preset graph {expression:?}: {error}"));
    }
}

#[test]
fn graph_chains_preserve_stereo_and_apply_gain_after_filtering() {
    let output = process("volume=0.5,pan=stereo|c0=c0|c1=0*c1", pcm(0, 960, 0.4), 0.5);
    let samples = output
        .iter()
        .flat_map(|frame| frame.samples.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(
        output
            .iter()
            .map(|frame| frame.samples_per_channel)
            .sum::<usize>(),
        960
    );
    assert_eq!(samples.len(), 1920);
    assert!(
        samples
            .as_chunks::<2>()
            .0
            .iter()
            .all(|pair| (pair[0] - 0.1).abs() < 0.002 && pair[1].abs() < 0.002)
    );
    let mut next = 0;
    for frame in output {
        assert_eq!(frame.start_sample, next);
        next += frame.samples_per_channel as u64;
    }
}

#[test]
fn time_changing_graphs_produce_a_contiguous_filtered_timeline_and_drain() {
    let input = PcmFrame {
        samples: (0..48_000)
            .flat_map(|index| {
                let value = (std::f32::consts::TAU * 440.0 * index as f32 / 48_000.0).sin();
                [value, value]
            })
            .collect(),
        samples_per_channel: 48_000,
        start_sample: 0,
    };
    let frames = process("atempo=1.25", input, 1.0);
    let count = frames
        .iter()
        .map(|frame| frame.samples_per_channel)
        .sum::<usize>();
    assert!(
        (37_000..=40_000).contains(&count),
        "filtered samples: {count}"
    );
    let mut next = 0;
    for frame in frames {
        assert_eq!(frame.start_sample, next);
        next += frame.samples_per_channel as u64;
    }
}

#[test]
fn time_changing_graphs_keep_original_media_progress_separate_from_output_samples() {
    for (expression, output_range) in [
        ("asetrate=60000,aresample=48000", 36_000..=40_000),
        ("asetrate=38400,aresample=48000", 57_000..=63_000),
        ("atempo=1.25", 36_000..=40_000),
    ] {
        let mut processor = PcmProcessor::new(Some(expression), 0).unwrap();
        let mut last_source_position = None;
        for start in (0..48_000).step_by(960) {
            processor
                .write_frame(
                    pcm(start, 960, 0.25),
                    Some(start),
                    false,
                    1.0,
                    |_, source_position| {
                        last_source_position = source_position;
                        Ok(())
                    },
                )
                .unwrap();
        }
        processor
            .finish(1.0, |_, source_position| {
                last_source_position = source_position;
                Ok(())
            })
            .unwrap();

        assert!(
            output_range.contains(&processor.next_sample()),
            "{expression}"
        );
        assert_eq!(
            last_source_position,
            Some(super::super::decoder::SourcePosition {
                sample: 48_000,
                estimated: true,
            }),
            "{expression}"
        );
    }
}

#[test]
fn no_graph_bypasses_ffmpeg_and_master_gain_clips_finite_samples() {
    let mut processor = PcmProcessor::new(None, 120).unwrap();
    let mut output = Vec::new();
    processor
        .write_frame(
            pcm(12_345, 3, 0.75),
            Some(12_345),
            false,
            2.0,
            |frame, _| {
                output.push(frame);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(output[0].start_sample, 120);
    assert_eq!(output[0].samples, vec![1.0; 6]);

    processor
        .write_frame(pcm(12_348, 3, 0.5), Some(12_348), false, 0.0, |frame, _| {
            output.push(frame);
            Ok(())
        })
        .unwrap();
    assert_eq!(output[1].start_sample, 123);
    assert_eq!(output[1].samples, vec![0.0; 6]);
}

#[test]
fn master_gain_applies_zero_half_unity_and_one_and_a_half_with_clipping() {
    for (gain, expected) in [(0.0, 0.0), (0.5, 0.2), (1.0, 0.4), (1.5, 0.6)] {
        let output = process("anull", pcm(0, 8, 0.4), gain);
        assert!(
            output[0]
                .samples
                .iter()
                .all(|sample| (sample - expected).abs() < 0.0001)
        );
    }
    let clipped = process("anull", pcm(0, 8, 0.8), 1.5);
    assert!(clipped[0].samples.iter().all(|sample| *sample == 1.0));
}

#[test]
fn bass_and_stereo_modulation_change_generated_audio() {
    let low = process("bass=gain=8:frequency=110", sine(48_000, 80.0), 1.0);
    let high = process("bass=gain=8:frequency=110", sine(48_000, 1_000.0), 1.0);
    assert!(
        rms(&low) > rms(&high) * 1.3,
        "low={}, high={}",
        rms(&low),
        rms(&high)
    );

    let moving = process(
        "apulsator=mode=sine:hz=0.125:amount=0.7",
        sine(48_000, 440.0),
        1.0,
    );
    let stereo_difference = moving
        .iter()
        .flat_map(|frame| frame.samples.as_chunks::<2>().0.iter())
        .map(|pair| (pair[0] - pair[1]).abs())
        .sum::<f32>();
    assert!(stereo_difference > 1.0);
}

#[test]
fn tremolo_vibrato_and_pitch_speed_presets_process_and_drain_samples() {
    let input = sine(48_000, 440.0);
    let plain = process("anull", input.clone(), 1.0);
    let tremolo = process("tremolo=f=5:d=0.6", input.clone(), 1.0);
    assert!(rms(&tremolo) < rms(&plain) * 0.9);
    let vibrato = process("vibrato=f=5:d=0.5", input.clone(), 1.0);
    assert_ne!(samples(&vibrato), samples(&plain));

    let nightcore = process("asetrate=60000,aresample=48000", input.clone(), 1.0);
    let vaporwave = process("asetrate=38400,aresample=48000", input, 1.0);
    let nightcore_samples = sample_frames(&nightcore);
    let vaporwave_samples = sample_frames(&vaporwave);
    assert!((37_000..=40_000).contains(&nightcore_samples));
    assert!((58_000..=62_000).contains(&vaporwave_samples));
}

fn sine(samples_per_channel: usize, frequency: f32) -> PcmFrame {
    let samples = (0..samples_per_channel)
        .flat_map(|index| {
            let value = (std::f32::consts::TAU * frequency * index as f32 / 48_000.0).sin() * 0.2;
            [value, value]
        })
        .collect();
    PcmFrame {
        samples,
        samples_per_channel,
        start_sample: 0,
    }
}

fn samples(frames: &[PcmFrame]) -> Vec<f32> {
    frames
        .iter()
        .flat_map(|frame| frame.samples.iter().copied())
        .collect()
}

fn sample_frames(frames: &[PcmFrame]) -> usize {
    frames.iter().map(|frame| frame.samples_per_channel).sum()
}

fn rms(frames: &[PcmFrame]) -> f32 {
    let samples = samples(frames);
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

#[test]
fn filter_graph_does_not_create_a_machine_sized_worker_pool() {
    let graph = PcmFilterGraph::build("tremolo=f=5:d=0.2").unwrap();
    // SAFETY: read-only inspection of a live, exclusively owned graph.
    assert_eq!(unsafe { (*graph.graph.as_ptr()).nb_threads }, 1);
}
