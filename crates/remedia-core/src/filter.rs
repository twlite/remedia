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
        // Audio sessions already execute independently. FFmpeg's automatic
        // graph threading otherwise creates a machine-sized pool per graph,
        // even for tiny stereo blocks. Keep processing on the owning worker.
        // SAFETY: this newly allocated graph is exclusively owned, and its
        // threading policy is set before filters are added or configured.
        unsafe {
            (*graph.as_mut_ptr()).nb_threads = 1;
        }
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
        // FFmpeg 9 uses array-valued abuffersink options. ffmpeg-next's
        // set_sample_format/set_sample_rate helpers still use older option
        // names and ignore errors, so configure the sink explicitly.
        let sink_args = format!(
            "sample_formats=flt:samplerates={OUTPUT_SAMPLE_RATE_HZ}:channel_layouts=stereo"
        );
        graph
            .add(&sink, "out", &sink_args)
            .map_err(|error| FilterGraphError::Invalid(error.to_string()))?;

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
                    return Err(FilterGraphError::Processing(format!(
                        "FFmpeg filter output has format {:?}, layout {:?}, and rate {} Hz instead of packed 48 kHz stereo f32 PCM",
                        frame.format(),
                        frame.channel_layout(),
                        frame.rate(),
                    )));
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
#[cfg(test)]
mod tests;

mod processor;
pub(crate) use processor::*;
