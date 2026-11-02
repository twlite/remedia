use std::ffi::{CStr, CString};

use ffmpeg_next as ffmpeg;
use thiserror::Error;

use crate::decoder::{OUTPUT_SAMPLE_RATE_HZ, PcmFrame, SourcePosition};
use crate::output::OutputError;

/// A filtergraph could not be built or processed for normalized PCM.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FilterGraphError {
    #[error("an audio filtergraph must not be empty or whitespace-only")]
    Empty,
    #[error("audio filtergraph contains a NUL byte")]
    NulByte,
    #[error("audio filtergraph is invalid or incompatible with one stereo PCM input/output: {0}")]
    Invalid(String),
    #[error("audio filtergraph processing failed: {0}")]
    Processing(String),
}

/// One native FFmpeg graph connected to normalized interleaved stereo PCM.
///
/// The graph itself is only called by the owning playback worker. `ffmpeg-next`
/// marks its graph handle Send; the wrapper does not expose its raw pointers.
pub(crate) struct PcmFilterGraph {
    graph: ffmpeg::filter::Graph,
    expression: String,
    eof_sent: bool,
}

impl PcmFilterGraph {
    pub(crate) fn build(expression: &str) -> Result<Self, FilterGraphError> {
        let _log_capture = crate::ffmpeg_logging::capture();
        if expression.trim().is_empty() {
            return Err(FilterGraphError::Empty);
        }
        if CString::new(expression).is_err() {
            return Err(FilterGraphError::NulByte);
        }
        crate::input::initialize().map_err(|error| FilterGraphError::Invalid(error.to_string()))?;

        let mut graph = ffmpeg::filter::Graph::new();
        let source = ffmpeg::filter::find("abuffer")
            .ok_or_else(|| FilterGraphError::Invalid("FFmpeg abuffer is unavailable".into()))?;
        let sink = ffmpeg::filter::find("abuffersink")
            .ok_or_else(|| FilterGraphError::Invalid("FFmpeg abuffersink is unavailable".into()))?;
        let source_args = format!(
            "time_base=1/{OUTPUT_SAMPLE_RATE_HZ}:sample_rate={OUTPUT_SAMPLE_RATE_HZ}:sample_fmt=flt:channel_layout=stereo"
        );
        graph
            .add(&source, "in", &source_args)
            .map_err(|error| FilterGraphError::Invalid(error.to_string()))?;
        graph
            .add(&sink, "out", "")
            .map_err(|error| FilterGraphError::Invalid(error.to_string()))?;

        {
            let mut output = graph.get("out").ok_or_else(|| {
                FilterGraphError::Invalid("FFmpeg output endpoint is missing".into())
            })?;
            output.set_sample_format(ffmpeg::format::Sample::F32(
                ffmpeg::format::sample::Type::Packed,
            ));
            output.set_channel_layout(ffmpeg::ChannelLayout::STEREO);
            output.set_sample_rate(OUTPUT_SAMPLE_RATE_HZ);
        }

        graph
            .output("in", 0)
            .and_then(|parser| parser.input("out", 0))
            .and_then(|parser| parser.parse(expression))
            .map_err(|error| FilterGraphError::Invalid(error.to_string()))?;
        graph
            .validate()
            .map_err(|error| FilterGraphError::Invalid(error.to_string()))?;
        validate_single_input_output(&graph)?;

        Ok(Self {
            graph,
            expression: expression.to_owned(),
            eof_sent: false,
        })
    }

    pub(crate) fn expression(&self) -> &str {
        &self.expression
    }

    fn push(&mut self, frame: &PcmFrame) -> Result<(), FilterGraphError> {
        let mut audio = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            frame.samples_per_channel,
            ffmpeg::ChannelLayout::STEREO,
        );
        audio.set_rate(OUTPUT_SAMPLE_RATE_HZ);
        audio.set_pts(Some(i64::try_from(frame.start_sample).map_err(|_| {
            FilterGraphError::Processing("input sample position exceeds FFmpeg's timeline".into())
        })?));

        let bytes = frame
            .samples
            .len()
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| FilterGraphError::Processing("PCM frame size overflowed".into()))?;
        let output = audio.data_mut(0);
        let output = output.get_mut(..bytes).ok_or_else(|| {
            FilterGraphError::Processing("FFmpeg allocated an undersized input frame".into())
        })?;
        let (sample_bytes, remainder) = output.as_chunks_mut::<4>();
        if !remainder.is_empty() {
            return Err(FilterGraphError::Processing(
                "FFmpeg allocated a misaligned input frame".into(),
            ));
        }
        for (destination, sample) in sample_bytes.iter_mut().zip(&frame.samples) {
            destination.copy_from_slice(&sample.to_ne_bytes());
        }

        let mut source = self.graph.get("in").ok_or_else(|| {
            FilterGraphError::Processing("FFmpeg input endpoint is missing".into())
        })?;
        source
            .source()
            .add(&audio)
            .map_err(|error| FilterGraphError::Processing(error.to_string()))
    }

    fn signal_eof(&mut self) -> Result<(), FilterGraphError> {
        if self.eof_sent {
            return Ok(());
        }
        let mut source = self.graph.get("in").ok_or_else(|| {
            FilterGraphError::Processing("FFmpeg input endpoint is missing".into())
        })?;
        source
            .source()
            .flush()
            .map_err(|error| FilterGraphError::Processing(error.to_string()))?;
        self.eof_sent = true;
        Ok(())
    }

    fn pull(&mut self) -> Result<Option<PcmFrame>, FilterGraphError> {
        let mut frame = ffmpeg::frame::Audio::empty();
        let result = {
            let mut output = self.graph.get("out").ok_or_else(|| {
                FilterGraphError::Processing("FFmpeg output endpoint is missing".into())
            })?;
            output.sink().frame(&mut frame)
        };

        match result {
            Ok(()) => {
                let expected_format =
                    ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed);
                if frame.format() != expected_format
                    || frame.channel_layout() != ffmpeg::ChannelLayout::STEREO
                    || frame.rate() != OUTPUT_SAMPLE_RATE_HZ
                {
                    return Err(FilterGraphError::Processing(
                        "FFmpeg filter output did not preserve packed 48 kHz stereo f32 PCM".into(),
                    ));
                }
                let samples_per_channel = frame.samples();
                let scalars = samples_per_channel.checked_mul(2).ok_or_else(|| {
                    FilterGraphError::Processing("filtered PCM sample count overflowed".into())
                })?;
                let byte_count = scalars.checked_mul(4).ok_or_else(|| {
                    FilterGraphError::Processing("filtered PCM frame size overflowed".into())
                })?;
                let bytes = frame.data(0).get(..byte_count).ok_or_else(|| {
                    FilterGraphError::Processing("FFmpeg returned an undersized PCM frame".into())
                })?;
                let samples = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| f32::from_ne_bytes(*bytes))
                    .collect();
                Ok(Some(PcmFrame {
                    samples,
                    samples_per_channel,
                    start_sample: 0,
                }))
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => Ok(None),
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(error) => Err(FilterGraphError::Processing(error.to_string())),
        }
    }
}

fn validate_single_input_output(graph: &ffmpeg::filter::Graph) -> Result<(), FilterGraphError> {
    // SAFETY: this graph was allocated and configured by ffmpeg-next and stays
    // immutably borrowed for the duration of this inspection. FFmpeg owns the
    // graph, filter array, contexts, and NUL-terminated filter names. Every
    // pointer is checked before dereferencing; the array length comes from the
    // same live AVFilterGraph.
    unsafe {
        let graph_ptr = graph.as_ptr();
        if graph_ptr.is_null() {
            return Err(FilterGraphError::Invalid(
                "FFmpeg returned a null graph".into(),
            ));
        }
        let graph_ref = &*graph_ptr;
        if graph_ref.nb_filters > 0 && graph_ref.filters.is_null() {
            return Err(FilterGraphError::Invalid(
                "FFmpeg returned a null filter list".into(),
            ));
        }
        let mut input_count = 0_u32;
        let mut output_count = 0_u32;
        for index in 0..graph_ref.nb_filters as usize {
            let context = *graph_ref.filters.add(index);
            if context.is_null() || (*context).filter.is_null() {
                return Err(FilterGraphError::Invalid(
                    "FFmpeg returned an invalid filter".into(),
                ));
            }
            let name = (*(*context).filter).name;
            if name.is_null() {
                return Err(FilterGraphError::Invalid(
                    "FFmpeg returned an unnamed filter".into(),
                ));
            }
            let name = CStr::from_ptr(name).to_bytes();
            if (*context).nb_inputs == 0 {
                input_count += 1;
                if name != b"abuffer" {
                    return Err(FilterGraphError::Invalid(
                        "filtergraph adds a separate audio source".into(),
                    ));
                }
            }
            if (*context).nb_outputs == 0 {
                output_count += 1;
                if name != b"abuffersink" {
                    return Err(FilterGraphError::Invalid(
                        "filtergraph adds a separate audio sink".into(),
                    ));
                }
            }
        }
        if input_count != 1 || output_count != 1 {
            return Err(FilterGraphError::Invalid(format!(
                "expected exactly one audio input and output, found {input_count} inputs and {output_count} outputs"
            )));
        }
    }
    Ok(())
}

/// Maintains a contiguous output timeline independently of input/source PTS.
pub(crate) struct PcmProcessor {
    graph: Option<PcmFilterGraph>,
    next_sample: u64,
    source_position: Option<SourcePosition>,
}

impl PcmProcessor {
    pub(crate) fn new(
        expression: Option<&str>,
        start_sample: u64,
    ) -> Result<Self, FilterGraphError> {
        Ok(Self {
            graph: expression.map(PcmFilterGraph::build).transpose()?,
            next_sample: start_sample,
            source_position: None,
        })
    }

    pub(crate) fn next_sample(&self) -> u64 {
        self.next_sample
    }

    pub(crate) fn expression(&self) -> Option<&str> {
        self.graph.as_ref().map(PcmFilterGraph::expression)
    }

    pub(crate) fn replace(&mut self, graph: Option<PcmFilterGraph>) {
        self.graph = graph;
    }

    pub(crate) fn reset(&mut self, start_sample: u64) -> Result<(), FilterGraphError> {
        let expression = self.expression().map(str::to_owned);
        self.graph = expression
            .as_deref()
            .map(PcmFilterGraph::build)
            .transpose()?;
        self.next_sample = start_sample;
        self.source_position = Some(SourcePosition {
            sample: start_sample,
            estimated: true,
        });
        Ok(())
    }

    pub(crate) fn write_frame(
        &mut self,
        input: PcmFrame,
        source_start_sample: Option<u64>,
        source_position_estimated: bool,
        gain: f32,
        mut deliver: impl FnMut(PcmFrame, Option<SourcePosition>) -> Result<(), OutputError>,
    ) -> Result<(), ProcessError> {
        let source_position_estimated = source_position_estimated || self.graph.is_some();
        let source_position = source_start_sample.map(|sample| SourcePosition {
            sample: sample.saturating_add(input.samples_per_channel as u64),
            estimated: source_position_estimated,
        });
        if source_position.is_some() {
            self.source_position = source_position;
        }
        if self.graph.is_some() {
            self.graph
                .as_mut()
                .expect("graph existence checked")
                .push(&input)
                .map_err(ProcessError::Filter)?;
            loop {
                let frame = self
                    .graph
                    .as_mut()
                    .expect("graph existence checked")
                    .pull()
                    .map_err(ProcessError::Filter)?;
                match frame {
                    Some(frame) => {
                        self.deliver(frame, gain, true, source_position, &mut deliver)?
                    }
                    None => break,
                }
            }
        } else {
            self.deliver(input, gain, false, source_position, &mut deliver)?;
        }
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        gain: f32,
        mut deliver: impl FnMut(PcmFrame, Option<SourcePosition>) -> Result<(), OutputError>,
    ) -> Result<(), ProcessError> {
        if self.graph.is_none() {
            return Ok(());
        }
        self.graph
            .as_mut()
            .expect("graph existence checked")
            .signal_eof()
            .map_err(ProcessError::Filter)?;
        loop {
            let frame = self
                .graph
                .as_mut()
                .expect("graph existence checked")
                .pull()
                .map_err(ProcessError::Filter)?;
            match frame {
                Some(frame) => {
                    self.deliver(frame, gain, true, self.source_position, &mut deliver)?
                }
                None => return Ok(()),
            }
        }
    }

    fn deliver(
        &mut self,
        mut frame: PcmFrame,
        gain: f32,
        clip_output: bool,
        source_position: Option<SourcePosition>,
        deliver: &mut impl FnMut(PcmFrame, Option<SourcePosition>) -> Result<(), OutputError>,
    ) -> Result<(), ProcessError> {
        let scalar_count = frame.samples_per_channel.checked_mul(2).ok_or_else(|| {
            ProcessError::Filter(FilterGraphError::Processing(
                "filtered PCM sample count overflowed".into(),
            ))
        })?;
        if scalar_count != frame.samples.len() {
            return Err(ProcessError::Filter(FilterGraphError::Processing(
                "FFmpeg returned PCM that is not interleaved stereo".into(),
            )));
        }
        if gain != 1.0 || clip_output {
            for sample in &mut frame.samples {
                let adjusted = *sample * gain;
                *sample = if adjusted.is_finite() {
                    adjusted.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
        }
        frame.start_sample = self.next_sample;
        self.next_sample = self
            .next_sample
            .saturating_add(u64::try_from(frame.samples_per_channel).unwrap_or(u64::MAX));
        deliver(frame, source_position).map_err(ProcessError::Output)
    }
}

#[derive(Debug)]
pub(crate) enum ProcessError {
    Filter(FilterGraphError),
    Output(OutputError),
}

#[cfg(test)]
mod tests {
    use super::{FilterGraphError, PcmProcessor};
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
            "volume=0.75,bass=g=8:f=110",
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
        let low = process("bass=g=8:f=110", sine(48_000, 80.0), 1.0);
        let high = process("bass=g=8:f=110", sine(48_000, 1_000.0), 1.0);
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
                let value =
                    (std::f32::consts::TAU * frequency * index as f32 / 48_000.0).sin() * 0.2;
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
}
