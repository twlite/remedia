use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use ffmpeg::codec::decoder::Audio as AudioCodecDecoder;
use ffmpeg::software::resampling::Context as Resampler;
use ffmpeg::util::format::sample::Type as SampleType;
use ffmpeg::{ChannelLayout, Rational};
use ffmpeg_next as ffmpeg;

use crate::error::{DecoderError, ffmpeg_error};
use crate::input::{self, OpenedInput};
use crate::source::{CancellationToken, InputSource};

const OUTPUT_SAMPLE_RATE: u32 = 48_000;
const OUTPUT_CHANNELS: u16 = 2;
const OUTPUT_SAMPLE_FORMAT: ffmpeg::format::Sample =
    ffmpeg::format::Sample::F32(SampleType::Planar);
const RESAMPLER_PADDING_SAMPLES: usize = 128;

/// The normalized PCM format emitted by this decoder: 48 kHz, stereo FL/FR,
/// 32-bit floating point, interleaved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    sample_rate: u32,
    channels: u16,
}

impl PcmFormat {
    pub const NORMALIZED: Self = Self {
        sample_rate: OUTPUT_SAMPLE_RATE,
        channels: OUTPUT_CHANNELS,
    };

    /// The normalized output sample rate, in samples per second.
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// The normalized output channel count.
    pub const fn channels(self) -> u16 {
        self.channels
    }
}

/// Options controlling native input reads and cancellation.
#[derive(Clone, Debug)]
pub struct DecoderOptions {
    /// Maximum time FFmpeg may wait on an individual network read, in addition
    /// to cancellation checks. Local files are not subject to this timeout.
    pub io_timeout: Duration,
    /// A clone may be retained by another thread and cancelled while `open`
    /// or `next_frame` is blocked in FFmpeg.
    pub cancellation: CancellationToken,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            io_timeout: Duration::from_secs(15),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Metadata discovered for the selected audio stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioStreamInfo {
    pub stream_index: usize,
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    /// FFmpeg's channel-layout description, such as `stereo` or `5.1(side)`.
    /// `None` means the source did not provide a recognized layout.
    pub channel_layout: Option<String>,
    pub duration: Option<Duration>,
    pub bit_rate: Option<u64>,
    pub metadata: BTreeMap<String, String>,
}

/// One owned block of normalized PCM. Samples are interleaved as
/// `L0, R0, L1, R1, ...` and each value is clamped to `[-1.0, 1.0]`.
///
/// The PCM format is always [`PcmFormat::NORMALIZED`]. `start_sample` is the
/// frame's position on the 48 kHz output timeline. The frame length is variable
/// and is never paced or padded to a fixed duration.
#[derive(Clone, Debug, PartialEq)]
pub struct PcmFrame {
    pub samples: Vec<f32>,
    pub samples_per_channel: usize,
    pub start_sample: u64,
}

/// A pull-based decoder that owns its FFmpeg input, codec, and resampler.
///
/// Each call to [`next_frame`](Self::next_frame) returns one decoded block as
/// soon as it is available. It does not pace playback or retain the whole
/// source in memory.
pub struct AudioDecoder {
    input: OpenedInput,
    decoder: AudioCodecDecoder,
    audio_stream_index: usize,
    network_input: bool,
    stream_time_base: Rational,
    info: AudioStreamInfo,
    cancellation: CancellationToken,
    resampler: Option<ResamplerState>,
    pending_frames: VecDeque<PcmFrame>,
    next_sample_position: Option<u64>,
    demux_finished: bool,
    finished: bool,
}

struct ResamplerState {
    context: Resampler,
    input_format: ffmpeg::format::Sample,
    input_layout: String,
    input_channels: u16,
    input_rate: u32,
}

impl AudioDecoder {
    /// Open a local file or an explicitly tagged HTTP(S) source with default
    /// network timeout and cancellation settings.
    pub fn open(source: InputSource) -> Result<Self, DecoderError> {
        Self::open_with_options(source, DecoderOptions::default())
    }

    /// Open a local file or an explicitly tagged HTTP(S) source.
    pub fn open_with_options(
        source: InputSource,
        options: DecoderOptions,
    ) -> Result<Self, DecoderError> {
        if options.io_timeout.is_zero() {
            return Err(DecoderError::InvalidOptions(
                "I/O timeout must be greater than zero",
            ));
        }
        if options.cancellation.is_cancelled() {
            return Err(DecoderError::Cancelled);
        }

        let network_input = matches!(&source, InputSource::HttpUrl(_));
        let input = input::open(&source, options.io_timeout, options.cancellation.clone())?;

        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Audio)
            .ok_or(DecoderError::NoAudioStream)?;
        let stream_index = stream.index();
        let stream_time_base = stream.time_base();
        let stream_duration = stream.duration();
        let parameters = stream.parameters();
        let codec_id = parameters.id();
        let stream_metadata = stream.metadata();
        let stream_tags: BTreeMap<String, String> = stream_metadata
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();

        let context = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(|error| decoder_open_error(error, codec_id.name(), &options.cancellation))?;
        let decoder = context
            .decoder()
            .audio()
            .map_err(|error| decoder_open_error(error, codec_id.name(), &options.cancellation))?;

        let mut metadata: BTreeMap<String, String> = input
            .metadata()
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        metadata.extend(stream_tags);

        let info = AudioStreamInfo {
            stream_index,
            codec: decoder
                .codec()
                .map(|codec| codec.name().to_owned())
                .unwrap_or_else(|| codec_id.name().to_owned()),
            sample_rate: (decoder.rate() > 0).then_some(decoder.rate()),
            channels: (decoder.channels() > 0).then_some(decoder.channels()),
            channel_layout: layout_description(decoder.channel_layout()),
            duration: stream_duration
                .checked_duration(stream_time_base)
                .or_else(|| duration_from_av_time_base(input.duration())),
            bit_rate: (decoder.bit_rate() > 0).then_some(decoder.bit_rate() as u64),
            metadata,
        };

        Ok(Self {
            input,
            decoder,
            audio_stream_index: stream_index,
            network_input,
            stream_time_base,
            info,
            cancellation: options.cancellation,
            resampler: None,
            pending_frames: VecDeque::new(),
            next_sample_position: None,
            demux_finished: false,
            finished: false,
        })
    }

    /// Return metadata for the selected audio stream.
    pub fn info(&self) -> &AudioStreamInfo {
        &self.info
    }

    /// Return the fixed normalized output format.
    pub const fn output_format(&self) -> PcmFormat {
        PcmFormat::NORMALIZED
    }

    /// Clone the token used to interrupt this decoder from another thread.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Decode and return the next normalized PCM block, or `Ok(None)` after
    /// the decoder has drained all codec and resampler delay.
    pub fn next_frame(&mut self) -> Result<Option<PcmFrame>, DecoderError> {
        self.check_cancelled()?;
        if let Some(frame) = self.pending_frames.pop_front() {
            return Ok(Some(frame));
        }
        if self.finished {
            return Ok(None);
        }

        loop {
            self.check_cancelled()?;

            if !self.demux_finished {
                let mut packet = ffmpeg::Packet::empty();
                match packet.read(&mut self.input) {
                    Ok(()) => {
                        if packet.stream() == self.audio_stream_index
                            && let Err(error) = self.decoder.send_packet(&packet)
                        {
                            if is_again(error) {
                                self.drain_decoder()?;
                                self.decoder.send_packet(&packet).map_err(|error| {
                                    self.decode_error(error, "sending an audio packet")
                                })?;
                            } else {
                                return Err(self.decode_error(error, "sending an audio packet"));
                            }
                        }
                    }
                    Err(ffmpeg::Error::Eof) => {
                        self.demux_finished = true;
                        self.decoder.send_eof().map_err(|error| {
                            self.decode_error(error, "signaling the end of the input")
                        })?;
                    }
                    Err(error) if is_again(error) => continue,
                    Err(error) => {
                        return Err(self.input_error(error, "reading media packets"));
                    }
                }
            }

            self.drain_decoder()?;
            if let Some(frame) = self.pending_frames.pop_front() {
                return Ok(Some(frame));
            }

            if self.demux_finished {
                self.flush_resampler()?;
                self.finished = true;
                return Ok(self.pending_frames.pop_front());
            }
        }
    }

    fn drain_decoder(&mut self) -> Result<(), DecoderError> {
        loop {
            self.check_cancelled()?;
            let mut decoded = ffmpeg::frame::Audio::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => self.convert_frame(&decoded)?,
                Err(ffmpeg::Error::Eof) => {
                    return Ok(());
                }
                Err(error) if is_again(error) => {
                    return Ok(());
                }
                Err(error) => return Err(self.decode_error(error, "receiving a decoded frame")),
            }
        }
    }

    fn convert_frame(&mut self, decoded: &ffmpeg::frame::Audio) -> Result<(), DecoderError> {
        if decoded.samples() == 0 {
            return Ok(());
        }

        let input_format = decoded.format();
        let input_rate = decoded.rate();
        let input_channels = decoded.channels();
        if input_format == ffmpeg::format::Sample::None || input_rate == 0 || input_channels == 0 {
            return Err(DecoderError::InvalidAudioFormat);
        }

        let mut input_layout = decoded.channel_layout();
        if input_layout.is_empty() {
            input_layout = ChannelLayout::default(i32::from(input_channels));
        }
        if input_layout.channels() <= 0 {
            return Err(DecoderError::InvalidAudioFormat);
        }
        let input_layout_key = layout_description(input_layout)
            .unwrap_or_else(|| format!("{} channels", input_layout.channels()));

        if let Some(resampler) = &self.resampler {
            if resampler.input_format != input_format
                || resampler.input_layout != input_layout_key
                || resampler.input_channels != input_channels
                || resampler.input_rate != input_rate
            {
                return Err(DecoderError::FormatChanged);
            }
        } else {
            let context = Resampler::get(
                input_format,
                input_layout,
                input_rate,
                OUTPUT_SAMPLE_FORMAT,
                ChannelLayout::STEREO,
                OUTPUT_SAMPLE_RATE,
            )
            .map_err(|error| DecoderError::Resample(error.to_string()))?;
            self.resampler = Some(ResamplerState {
                context,
                input_format,
                input_layout: input_layout_key,
                input_channels,
                input_rate,
            });
        }

        let normalized_input = frame_with_channel_layout(decoded, input_layout)?;
        let resampler = self.resampler.as_mut().expect("resampler initialized");
        let delay = resampler_delay_output(&resampler.context);
        let capacity = output_capacity(decoded.samples(), input_rate, delay)?;
        let mut output =
            ffmpeg::frame::Audio::new(OUTPUT_SAMPLE_FORMAT, capacity, ChannelLayout::STEREO);
        output.set_rate(OUTPUT_SAMPLE_RATE);
        resampler
            .context
            .run(&normalized_input, &mut output)
            .map_err(|error| DecoderError::Resample(error.to_string()))?;
        self.enqueue_output(&output, decoded.timestamp())
    }

    fn flush_resampler(&mut self) -> Result<(), DecoderError> {
        let mut output_frames = Vec::new();
        if let Some(resampler) = self.resampler.as_mut() {
            let delay = resampler_delay_output(&resampler.context);
            if delay > 0 {
                let capacity = delay
                    .checked_add(RESAMPLER_PADDING_SAMPLES)
                    .ok_or(DecoderError::FrameTooLarge)?;
                let mut output = ffmpeg::frame::Audio::new(
                    OUTPUT_SAMPLE_FORMAT,
                    capacity,
                    ChannelLayout::STEREO,
                );
                output.set_rate(OUTPUT_SAMPLE_RATE);
                resampler
                    .context
                    .flush(&mut output)
                    .map_err(|error| DecoderError::Resample(error.to_string()))?;
                output_frames.push(output);
            }
        }

        for output in output_frames {
            self.enqueue_output(&output, None)?;
        }
        Ok(())
    }

    fn enqueue_output(
        &mut self,
        output: &ffmpeg::frame::Audio,
        source_timestamp: Option<i64>,
    ) -> Result<(), DecoderError> {
        let samples_per_channel = output.samples();
        if samples_per_channel == 0 {
            return Ok(());
        }
        if output.planes() < usize::from(OUTPUT_CHANNELS) {
            return Err(DecoderError::InvalidAudioFormat);
        }

        let left = output.plane::<f32>(0);
        let right = output.plane::<f32>(1);
        if left.len() < samples_per_channel || right.len() < samples_per_channel {
            return Err(DecoderError::InvalidAudioFormat);
        }

        let start_sample = *self.next_sample_position.get_or_insert_with(|| {
            timestamp_to_output_sample(source_timestamp, self.stream_time_base, OUTPUT_SAMPLE_RATE)
                .unwrap_or(0)
        });
        let sample_count = samples_per_channel
            .checked_mul(usize::from(OUTPUT_CHANNELS))
            .ok_or(DecoderError::FrameTooLarge)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(sample_count)
            .map_err(|_| DecoderError::FrameTooLarge)?;
        for index in 0..samples_per_channel {
            samples.push(normalize_sample(left[index]));
            samples.push(normalize_sample(right[index]));
        }

        self.next_sample_position = Some(start_sample.saturating_add(samples_per_channel as u64));
        self.pending_frames.push_back(PcmFrame {
            samples,
            samples_per_channel,
            start_sample,
        });
        Ok(())
    }

    fn check_cancelled(&self) -> Result<(), DecoderError> {
        if self.cancellation.is_cancelled() {
            Err(DecoderError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn decode_error(&self, error: ffmpeg::Error, operation: &'static str) -> DecoderError {
        let error = ffmpeg_error(error, operation, self.cancellation.is_cancelled(), false);
        match error {
            DecoderError::OpenInput(message) => DecoderError::Decode(message),
            other => other,
        }
    }

    fn input_error(&self, error: ffmpeg::Error, operation: &'static str) -> DecoderError {
        ffmpeg_error(
            error,
            operation,
            self.cancellation.is_cancelled(),
            self.network_input,
        )
    }
}

fn decoder_open_error(
    error: ffmpeg::Error,
    codec: &str,
    cancellation: &CancellationToken,
) -> DecoderError {
    if cancellation.is_cancelled() {
        return DecoderError::Cancelled;
    }
    if matches!(error, ffmpeg::Error::DecoderNotFound) {
        DecoderError::DecoderUnavailable(codec.to_owned())
    } else {
        DecoderError::DecoderInitialization(format!("{codec}: {error}"))
    }
}

fn is_again(error: ffmpeg::Error) -> bool {
    matches!(error, ffmpeg::Error::Other { errno } if errno == ffmpeg::error::EAGAIN)
}

fn output_capacity(
    input_samples: usize,
    input_rate: u32,
    delay: usize,
) -> Result<usize, DecoderError> {
    let numerator = (input_samples as u128)
        .checked_mul(u128::from(OUTPUT_SAMPLE_RATE))
        .ok_or(DecoderError::FrameTooLarge)?;
    let converted = numerator
        .checked_add(u128::from(input_rate) - 1)
        .ok_or(DecoderError::FrameTooLarge)?
        / u128::from(input_rate);
    let capacity = converted
        .checked_add(delay as u128)
        .and_then(|capacity| capacity.checked_add(RESAMPLER_PADDING_SAMPLES as u128))
        .ok_or(DecoderError::FrameTooLarge)?;
    let capacity = usize::try_from(capacity).map_err(|_| DecoderError::FrameTooLarge)?;
    if capacity > i32::MAX as usize {
        return Err(DecoderError::FrameTooLarge);
    }
    Ok(capacity)
}

fn resampler_delay_output(context: &Resampler) -> usize {
    // SAFETY: `context` owns a live SwrContext and the call only reads its
    // current delay. Decoder access is exclusive while this helper runs.
    let delay = unsafe {
        ffmpeg::ffi::swr_get_delay(context.as_ptr() as *mut _, i64::from(OUTPUT_SAMPLE_RATE))
    };
    delay.max(0) as usize
}

fn frame_with_channel_layout(
    source: &ffmpeg::frame::Audio,
    channel_layout: ChannelLayout,
) -> Result<ffmpeg::frame::Audio, DecoderError> {
    let mut normalized = ffmpeg::frame::Audio::empty();
    // SAFETY: both frames are valid FFmpeg frames. `av_frame_ref` retains the
    // source buffers in `normalized`, so they remain alive for the conversion;
    // dropping the new frame releases only its own references.
    let result = unsafe { ffmpeg::ffi::av_frame_ref(normalized.as_mut_ptr(), source.as_ptr()) };
    if result < 0 {
        return Err(DecoderError::Resample(
            ffmpeg::Error::from(result).to_string(),
        ));
    }
    if normalized.channel_layout().is_empty() {
        normalized.set_channel_layout(channel_layout);
    }
    Ok(normalized)
}

fn normalize_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn timestamp_to_output_sample(
    timestamp: Option<i64>,
    time_base: Rational,
    sample_rate: u32,
) -> Option<u64> {
    let timestamp = timestamp?;
    if timestamp < 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
        return None;
    }

    let numerator = i128::from(timestamp)
        .checked_mul(i128::from(time_base.numerator()))?
        .checked_mul(i128::from(sample_rate))?;
    let samples = numerator / i128::from(time_base.denominator());
    u64::try_from(samples).ok()
}

fn duration_from_av_time_base(duration: i64) -> Option<Duration> {
    (duration > 0).then(|| Duration::from_micros(duration as u64))
}

trait CheckedDuration {
    fn checked_duration(self, time_base: Rational) -> Option<Duration>;
}

impl CheckedDuration for i64 {
    fn checked_duration(self, time_base: Rational) -> Option<Duration> {
        if self <= 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
            return None;
        }
        let seconds =
            self as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator());
        Duration::try_from_secs_f64(seconds).ok()
    }
}

fn layout_description(layout: ChannelLayout) -> Option<String> {
    if layout.is_empty() || layout.channels() <= 0 {
        return None;
    }
    let mut buffer = [0_i8; 128];
    // SAFETY: the buffer is writable for the supplied size and `layout` is a
    // copy of a valid FFmpeg channel layout owned by the decoded stream.
    let result = unsafe {
        ffmpeg::ffi::av_channel_layout_describe(&layout.into(), buffer.as_mut_ptr(), buffer.len())
    };
    if result < 0 {
        return None;
    }
    // SAFETY: FFmpeg writes a NUL-terminated description when it succeeds.
    let description = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(description.to_string_lossy().into_owned())
}
