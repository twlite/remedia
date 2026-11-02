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

pub(crate) const OUTPUT_SAMPLE_RATE_HZ: u32 = 48_000;
const OUTPUT_CHANNELS: u16 = 2;
const OUTPUT_SAMPLE_FORMAT: ffmpeg::format::Sample =
    ffmpeg::format::Sample::F32(SampleType::Planar);
const RESAMPLER_PADDING_SAMPLES: usize = 128;
const OPUS_PREROLL_SAMPLES: u64 = 5_760;
const OPUS_HISTORY_MAX_BYTES: usize = 512 * 1024;

/// The normalized PCM format emitted by this decoder: 48 kHz, stereo FL/FR,
/// 32-bit floating point, interleaved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    sample_rate: u32,
    channels: u16,
}

impl PcmFormat {
    pub const NORMALIZED: Self = Self {
        sample_rate: OUTPUT_SAMPLE_RATE_HZ,
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

/// Internal decoder frame plus its best available source-timeline timestamp.
/// The public PCM frame continues to expose only the normalized output shape.
#[derive(Clone, Debug)]
pub(crate) struct DecodedPcmFrame {
    pub frame: PcmFrame,
    pub source_start_sample: Option<u64>,
    pub source_position_estimated: bool,
}

/// A source position associated with a processed output boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourcePosition {
    pub sample: u64,
    pub estimated: bool,
}

/// A pull-based decoder that owns its FFmpeg input, codec, and resampler.
///
/// Each call to [`next_frame`](Self::next_frame) returns one decoded block as
/// soon as it is available. It does not pace playback or retain the whole
/// source in memory.
pub struct AudioDecoder {
    reader: AudioPacketReader,
    decoder: Option<AudioCodecDecoder>,
    cancellation: CancellationToken,
    resampler: Option<ResamplerState>,
    pending_frames: VecDeque<DecodedPcmFrame>,
    next_sample_position: Option<u64>,
    seek_target_sample: Option<u64>,
    finished: bool,
}

/// Owns the one FFmpeg demux context and selected stream. It can read
/// compressed audio packets without opening a codec; PCM decoding is attached
/// lazily when a consumer needs it.
pub(crate) struct AudioPacketReader {
    input: OpenedInput,
    codec_parameters: ffmpeg::codec::Parameters,
    audio_stream_index: usize,
    network_input: bool,
    stream_time_base: Rational,
    info: AudioStreamInfo,
    cancellation: CancellationToken,
    pending_packets: VecDeque<DemuxedAudioPacket>,
    next_source_sample: Option<u64>,
    stream_start_sample: Option<u64>,
    demux_finished: bool,
    passthrough_rejection: Option<&'static str>,
    packet_history: VecDeque<DemuxedAudioPacket>,
    packet_history_samples: u64,
    packet_history_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct DemuxedAudioPacket {
    pub packet: ffmpeg::Packet,
    pub source_start_sample: Option<u64>,
    pub source_position_estimated: bool,
    pub samples_per_channel: Option<u32>,
    pub has_trimming_metadata: bool,
}

struct ResamplerState {
    context: Resampler,
    input_format: ffmpeg::format::Sample,
    input_layout: String,
    input_channels: u16,
    input_rate: u32,
}

impl AudioPacketReader {
    fn open(source: InputSource, options: DecoderOptions) -> Result<Self, DecoderError> {
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
        let stream_start_time = stream.start_time();
        let parameters = stream.parameters();
        let codec_id = parameters.id();
        let codec_parameters = parameters.clone();
        let stream_tags = stream
            .metadata()
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        let (sample_rate, channels, bit_rate, channel_layout, initial_padding, trailing_padding) =
            codec_parameter_metadata(&codec_parameters);
        let mut metadata = input
            .metadata()
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        metadata.extend(stream_tags);
        let info = AudioStreamInfo {
            stream_index,
            codec: codec_id.name().to_owned(),
            sample_rate,
            channels,
            channel_layout,
            duration: stream_duration
                .checked_duration(stream_time_base)
                .or_else(|| duration_from_av_time_base(input.duration())),
            bit_rate,
            metadata,
        };
        let stream_start_sample = timestamp_to_output_sample(
            (stream_start_time != ffmpeg::ffi::AV_NOPTS_VALUE).then_some(stream_start_time),
            stream_time_base,
            OUTPUT_SAMPLE_RATE_HZ,
        );
        let passthrough_rejection = if codec_id != ffmpeg::codec::Id::OPUS {
            Some("selected codec is not Opus")
        } else if sample_rate != Some(OUTPUT_SAMPLE_RATE_HZ) {
            Some("Opus stream sample rate is not 48 kHz")
        } else if !matches!(channels, Some(1 | 2)) {
            Some("Opus channel layout is not mono or stereo")
        } else if initial_padding != 0 || trailing_padding != 0 {
            Some("stream requires initial or trailing sample trimming")
        } else {
            opus_head_passthrough_rejection(&codec_parameters, channels)
        };
        Ok(Self {
            input,
            codec_parameters,
            audio_stream_index: stream_index,
            network_input,
            stream_time_base,
            info,
            cancellation: options.cancellation,
            pending_packets: VecDeque::new(),
            next_source_sample: Some(0),
            stream_start_sample,
            demux_finished: false,
            passthrough_rejection,
            packet_history: VecDeque::new(),
            packet_history_samples: 0,
            packet_history_bytes: 0,
        })
    }

    fn opus_passthrough_rejection(&self) -> Option<&'static str> {
        self.passthrough_rejection
    }

    fn source_boundary_sample(&self) -> Option<u64> {
        self.next_source_sample
    }

    fn history_before(&self, boundary_sample: u64) -> Vec<DemuxedAudioPacket> {
        self.packet_history
            .iter()
            .filter(|packet| {
                packet
                    .source_start_sample
                    .zip(packet.samples_per_channel)
                    .is_some_and(|(start, samples)| {
                        start.saturating_add(u64::from(samples)) <= boundary_sample
                    })
            })
            .cloned()
            .collect()
    }

    fn clear_packet_history(&mut self) {
        self.packet_history.clear();
        self.packet_history_samples = 0;
        self.packet_history_bytes = 0;
    }

    fn remember_packet(&mut self, packet: &DemuxedAudioPacket) {
        let Some(samples) = packet.samples_per_channel else {
            return;
        };
        let packet_bytes = packet.packet.size();
        if packet.has_trimming_metadata || packet_bytes > OPUS_HISTORY_MAX_BYTES {
            return;
        }
        self.packet_history.push_back(packet.clone());
        self.packet_history_samples = self
            .packet_history_samples
            .saturating_add(u64::from(samples));
        self.packet_history_bytes = self.packet_history_bytes.saturating_add(packet_bytes);
        while self.packet_history_samples > OPUS_PREROLL_SAMPLES
            || self.packet_history_bytes > OPUS_HISTORY_MAX_BYTES
        {
            let Some(oldest) = self.packet_history.pop_front() else {
                break;
            };
            self.packet_history_samples = self
                .packet_history_samples
                .saturating_sub(u64::from(oldest.samples_per_channel.unwrap_or(0)));
            self.packet_history_bytes = self
                .packet_history_bytes
                .saturating_sub(oldest.packet.size());
        }
    }

    fn read_audio_packet(&mut self) -> Result<Option<DemuxedAudioPacket>, DecoderError> {
        if let Some(packet) = self.pending_packets.pop_front() {
            return Ok(Some(packet));
        }
        if self.demux_finished {
            return Ok(None);
        }
        loop {
            if self.cancellation.is_cancelled() {
                return Err(DecoderError::Cancelled);
            }
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) if packet.stream() != self.audio_stream_index => continue,
                Ok(()) => {
                    let timestamp = packet.pts().or_else(|| packet.dts());
                    let timestamp_sample = timestamp_to_output_sample(
                        timestamp,
                        self.stream_time_base,
                        OUTPUT_SAMPLE_RATE_HZ,
                    )
                    .and_then(|sample| sample.checked_sub(self.stream_start_sample.unwrap_or(0)));
                    let source_start_sample = timestamp_sample.or(self.next_source_sample);
                    let samples = (self.codec_parameters.id() == ffmpeg::codec::Id::OPUS)
                        .then(|| {
                            packet.data().and_then(|bytes| {
                                opus2::packet::get_nb_samples(bytes, OUTPUT_SAMPLE_RATE_HZ).ok()
                            })
                        })
                        .flatten();
                    let samples_per_channel =
                        samples.and_then(|samples| u32::try_from(samples).ok());
                    let has_skip_samples = packet.side_data().any(|side_data| {
                        side_data.kind() == ffmpeg::codec::packet::side_data::Type::SkipSamples
                    });
                    let has_trimmed_duration = packet.duration() > 0
                        && samples_per_channel.is_some_and(|samples| {
                            timestamp_to_output_sample(
                                Some(packet.duration()),
                                self.stream_time_base,
                                OUTPUT_SAMPLE_RATE_HZ,
                            ) != Some(u64::from(samples))
                        });
                    if let (Some(start), Some(samples)) = (source_start_sample, samples_per_channel)
                    {
                        self.next_source_sample = Some(start.saturating_add(u64::from(samples)));
                    }
                    let demuxed = DemuxedAudioPacket {
                        packet,
                        source_start_sample,
                        source_position_estimated: timestamp_sample.is_none(),
                        samples_per_channel,
                        has_trimming_metadata: has_skip_samples || has_trimmed_duration,
                    };
                    if self.codec_parameters.id() == ffmpeg::codec::Id::OPUS {
                        self.remember_packet(&demuxed);
                    }
                    return Ok(Some(demuxed));
                }
                Err(ffmpeg::Error::Eof) => {
                    if let Some(message) = self.input.producer_failure() {
                        return Err(DecoderError::InputFailure(message));
                    }
                    self.demux_finished = true;
                    return Ok(None);
                }
                Err(error) if is_again(error) => continue,
                Err(error) => {
                    if let Some(message) = self.input.producer_failure() {
                        return Err(DecoderError::InputFailure(message));
                    }
                    return Err(ffmpeg_error(
                        error,
                        "reading media packets",
                        self.cancellation.is_cancelled(),
                        self.network_input,
                    ));
                }
            }
        }
    }
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
        let _log_capture = crate::ffmpeg_logging::capture();
        let source_kind = source.kind();
        let span = tracing::info_span!(
            target: "remedia_core::decoder",
            "core.media.open",
            source_kind = source_kind,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        span.in_scope(|| {
            let result = Self::open_inner(source, options);
            match &result {
                Ok(decoder) => {
                    span.record("otel.status_code", "OK");
                    tracing::info!(
                        name: "core.media.opened",
                        target: "remedia_core::decoder",
                        source_kind,
                        stream_index = decoder.info().stream_index,
                        codec = %decoder.info().codec,
                        source_sample_rate = decoder.info().sample_rate,
                        source_channels = decoder.info().channels,
                        duration_known = decoder.info().duration.is_some(),
                        "audio stream selected"
                    );
                }
                Err(error) => {
                    span.record("otel.status_code", "ERROR");
                    span.record("error.type", error.kind());
                    tracing::error!(
                        name: "core.media.open_failed",
                        target: "remedia_core::decoder",
                        source_kind,
                        error_kind = error.kind(),
                        "media input could not be opened"
                    );
                }
            }
            result
        })
    }

    fn open_inner(source: InputSource, options: DecoderOptions) -> Result<Self, DecoderError> {
        let mut decoder = Self::open_for_playback(source, options)?;
        decoder.activate_decoder(&[], None)?;
        Ok(decoder)
    }

    pub(crate) fn open_for_playback(
        source: InputSource,
        options: DecoderOptions,
    ) -> Result<Self, DecoderError> {
        let reader = AudioPacketReader::open(source, options)?;
        let cancellation = reader.cancellation.clone();
        Ok(Self {
            reader,
            decoder: None,
            cancellation,
            resampler: None,
            pending_frames: VecDeque::new(),
            next_sample_position: None,
            seek_target_sample: None,
            finished: false,
        })
    }

    pub(crate) fn opus_passthrough_rejection(&self) -> Option<&'static str> {
        self.reader.opus_passthrough_rejection()
    }

    pub(crate) fn read_compressed_packet(
        &mut self,
    ) -> Result<Option<DemuxedAudioPacket>, DecoderError> {
        self.check_cancelled()?;
        self.reader.read_audio_packet()
    }

    pub(crate) fn source_boundary_sample(&self) -> Option<u64> {
        self.reader.source_boundary_sample()
    }

    pub(crate) fn packet_history_before(&self, boundary_sample: u64) -> Vec<DemuxedAudioPacket> {
        self.reader.history_before(boundary_sample)
    }

    pub(crate) fn queue_compressed_packet(&mut self, packet: DemuxedAudioPacket) {
        self.reader.pending_packets.push_front(packet);
        self.reader.demux_finished = false;
        self.finished = false;
    }

    pub(crate) fn activate_decoder(
        &mut self,
        preroll: &[DemuxedAudioPacket],
        source_boundary: Option<u64>,
    ) -> Result<(), DecoderError> {
        self.check_cancelled()?;
        if self.decoder.is_some() {
            return Ok(());
        }
        let codec_id = self.reader.codec_parameters.id();
        let span = tracing::info_span!(
            target: "remedia_core::decoder",
            "core.decoder.initialize",
            codec = codec_id.name(),
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let result = span.in_scope(|| {
            let context = ffmpeg::codec::context::Context::from_parameters(
                self.reader.codec_parameters.clone(),
            )
            .map_err(|error| decoder_open_error(error, codec_id.name(), &self.cancellation))?;
            let mut decoder_context = context.decoder();
            if self.reader.stream_time_base.numerator() > 0
                && self.reader.stream_time_base.denominator() > 0
            {
                decoder_context.set_packet_time_base(self.reader.stream_time_base);
            }
            decoder_context
                .audio()
                .map_err(|error| decoder_open_error(error, codec_id.name(), &self.cancellation))
        });
        let decoder = match result {
            Ok(decoder) => {
                span.record("otel.status_code", "OK");
                decoder
            }
            Err(error) => {
                span.record("otel.status_code", "ERROR");
                span.record("error.type", error.kind());
                tracing::error!(
                    name: "core.decoder.initialization_failed",
                    target: "remedia_core::decoder",
                    codec = codec_id.name(),
                    error_kind = error.kind(),
                    "native audio decoder initialization failed"
                );
                return Err(error);
            }
        };
        self.decoder = Some(decoder);
        self.finished = false;
        self.seek_target_sample = source_boundary;

        if !preroll.is_empty() {
            tracing::debug!(
                name: "core.decoder.warmup_started",
                target: "remedia_core::decoder",
                codec = codec_id.name(),
                packet_count = preroll.len(),
                source_boundary_sample = source_boundary,
                "decoder warm-up replaying bounded Opus history"
            );
        }
        for packet in preroll {
            self.send_packet(&packet.packet)?;
            self.drain_decoder()?;
        }
        if !preroll.is_empty() {
            tracing::debug!(
                name: "core.decoder.warmup_completed",
                target: "remedia_core::decoder",
                codec = codec_id.name(),
                packet_count = preroll.len(),
                "decoder warm-up reached the transition boundary"
            );
        }
        Ok(())
    }

    pub(crate) fn deactivate_decoder(&mut self) -> Result<(), DecoderError> {
        if !self.pending_frames.is_empty() {
            return Err(DecoderError::Decode(
                "cannot leave PCM mode while decoded frames are pending".to_owned(),
            ));
        }
        self.decoder = None;
        self.resampler = None;
        self.next_sample_position = None;
        self.seek_target_sample = None;
        self.finished = false;
        tracing::debug!(
            name: "core.decoder.deactivated",
            target: "remedia_core::decoder",
            codec = self.reader.codec_parameters.id().name(),
            "native decoder and resampler released after passthrough transition"
        );
        Ok(())
    }

    /// Roll back an unsuccessful passthrough-to-PCM activation while retaining
    /// the demuxer and its unread input position.
    pub(crate) fn discard_decoder_activation(&mut self) {
        self.decoder = None;
        self.resampler = None;
        self.pending_frames.clear();
        self.next_sample_position = None;
        self.seek_target_sample = None;
        self.finished = false;
    }

    pub(crate) fn has_pending_frames(&self) -> bool {
        !self.pending_frames.is_empty()
    }

    pub(crate) fn read_next_timed_frame(
        &mut self,
    ) -> Result<Option<DecodedPcmFrame>, DecoderError> {
        self.next_timed_frame()
    }

    /// Return metadata for the selected audio stream.
    pub fn info(&self) -> &AudioStreamInfo {
        &self.reader.info
    }

    /// Return the fixed normalized output format.
    pub const fn output_format(&self) -> PcmFormat {
        PcmFormat::NORMALIZED
    }

    /// Clone the token used to interrupt this decoder from another thread.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Seek to a media position. FFmpeg may seek to an earlier keyframe; audio
    /// before the requested output sample is discarded after decoding. The
    /// operation fails with [`DecoderError::UnsupportedSeeking`] when the
    /// demuxer or source cannot seek.
    pub fn seek(&mut self, position: Duration) -> Result<(), DecoderError> {
        let _log_capture = crate::ffmpeg_logging::capture();
        self.check_cancelled()?;
        if !self.reader.input.is_seekable() {
            return Err(DecoderError::UnsupportedSeeking(
                "the byte stream is non-seekable".to_owned(),
            ));
        }
        if let Some(duration) = self.reader.info.duration
            && position > duration
        {
            return Err(DecoderError::SeekOutOfRange {
                target: position,
                duration,
            });
        }

        let timestamp = i64::try_from(position.as_micros()).map_err(|_| {
            DecoderError::UnsupportedSeeking(
                "the target exceeds FFmpeg's timestamp range".to_owned(),
            )
        })?;
        let target_sample = duration_to_sample_position(position).map_err(|_| {
            DecoderError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
        })?;
        if let Err(error) = self.reader.input.seek(timestamp, ..) {
            if self.cancellation.is_cancelled() {
                return Err(DecoderError::Cancelled);
            }
            return Err(DecoderError::UnsupportedSeeking(error.to_string()));
        }

        if let Some(decoder) = self.decoder.as_mut() {
            decoder.flush();
        }
        self.resampler = None;
        self.pending_frames.clear();
        self.next_sample_position = None;
        self.seek_target_sample = Some(target_sample);
        self.reader.demux_finished = false;
        self.reader.pending_packets.clear();
        self.reader.next_source_sample = Some(target_sample);
        self.reader.clear_packet_history();
        self.finished = false;
        Ok(())
    }

    /// Decode and return the next normalized PCM block, or `Ok(None)` after
    /// the decoder has drained all codec and resampler delay.
    pub fn next_frame(&mut self) -> Result<Option<PcmFrame>, DecoderError> {
        Ok(self.next_timed_frame()?.map(|frame| frame.frame))
    }

    pub(crate) fn next_timed_frame(&mut self) -> Result<Option<DecodedPcmFrame>, DecoderError> {
        let _log_capture = crate::ffmpeg_logging::capture();
        self.check_cancelled()?;
        if self.decoder.is_none() {
            self.activate_decoder(&[], None)?;
        }
        if let Some(frame) = self.pending_frames.pop_front() {
            return Ok(Some(frame));
        }
        if self.finished {
            return Ok(None);
        }

        loop {
            self.check_cancelled()?;

            if !self.reader.demux_finished {
                match self.read_compressed_packet()? {
                    Some(packet) => self.send_packet(&packet.packet)?,
                    None => self
                        .decoder
                        .as_mut()
                        .ok_or_else(|| DecoderError::Decode("decoder is inactive".into()))?
                        .send_eof()
                        .map_err(|error| {
                            self.decode_error(error, "signaling the end of the input")
                        })?,
                }
            }

            self.drain_decoder()?;
            if let Some(frame) = self.pending_frames.pop_front() {
                return Ok(Some(frame));
            }

            if self.reader.demux_finished {
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
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(());
            };
            match decoder.receive_frame(&mut decoded) {
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

    fn send_packet(&mut self, packet: &ffmpeg::Packet) -> Result<(), DecoderError> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Err(DecoderError::Decode("decoder is inactive".into()));
        };
        if let Err(error) = decoder.send_packet(packet) {
            if is_again(error) {
                self.drain_decoder()?;
                self.decoder
                    .as_mut()
                    .ok_or_else(|| DecoderError::Decode("decoder is inactive".into()))?
                    .send_packet(packet)
                    .map_err(|error| self.decode_error(error, "sending an audio packet"))?;
            } else {
                return Err(self.decode_error(error, "sending an audio packet"));
            }
        }
        Ok(())
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
        let input_layout_key = frame_layout_description(decoded)
            .or_else(|| layout_description_from_mask(input_layout))
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
                OUTPUT_SAMPLE_RATE_HZ,
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
        output.set_rate(OUTPUT_SAMPLE_RATE_HZ);
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
                output.set_rate(OUTPUT_SAMPLE_RATE_HZ);
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

        let source_start = timestamp_to_output_sample(
            source_timestamp,
            self.reader.stream_time_base,
            OUTPUT_SAMPLE_RATE_HZ,
        );
        let previous_next_sample = self.next_sample_position;
        let seek_target_sample = self.seek_target_sample;
        let (start_sample, trim_samples) = if let Some(target_sample) = seek_target_sample {
            let source_start = source_start.unwrap_or(target_sample);
            let source_end = source_start.saturating_add(samples_per_channel as u64);
            if source_end <= target_sample {
                return Ok(());
            }
            let trim_samples = target_sample.saturating_sub(source_start) as usize;
            let start_sample = source_start.max(target_sample);
            self.seek_target_sample = None;
            (start_sample, trim_samples)
        } else {
            (
                *self
                    .next_sample_position
                    .get_or_insert_with(|| source_start.unwrap_or(0)),
                0,
            )
        };
        let samples_per_channel = samples_per_channel.saturating_sub(trim_samples);
        if samples_per_channel == 0 {
            return Ok(());
        }
        let sample_count = samples_per_channel
            .checked_mul(usize::from(OUTPUT_CHANNELS))
            .ok_or(DecoderError::FrameTooLarge)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(sample_count)
            .map_err(|_| DecoderError::FrameTooLarge)?;
        for index in trim_samples..trim_samples + samples_per_channel {
            samples.push(normalize_sample(left[index]));
            samples.push(normalize_sample(right[index]));
        }

        let (source_start_sample, source_position_estimated) =
            if let Some(target_sample) = seek_target_sample {
                if let Some(source_start) = source_start {
                    (
                        Some(source_start.saturating_add(trim_samples as u64)),
                        false,
                    )
                } else {
                    (Some(target_sample), true)
                }
            } else if let Some(source_start) = source_start {
                (Some(source_start), false)
            } else if let Some(previous_sample) = previous_next_sample {
                (Some(previous_sample), true)
            } else {
                (None, true)
            };
        self.next_sample_position = Some(start_sample.saturating_add(samples_per_channel as u64));
        self.pending_frames.push_back(DecodedPcmFrame {
            frame: PcmFrame {
                samples,
                samples_per_channel,
                start_sample,
            },
            source_start_sample,
            source_position_estimated,
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

fn codec_parameter_metadata(
    parameters: &ffmpeg::codec::Parameters,
) -> (
    Option<u32>,
    Option<u16>,
    Option<u64>,
    Option<String>,
    i32,
    i32,
) {
    // SAFETY: Parameters owns a live AVCodecParameters allocation. The fields
    // read below are scalar values or a borrowed AVChannelLayout inspected
    // synchronously; no native pointer escapes this helper.
    let native = unsafe { &*parameters.as_ptr() };
    let sample_rate = u32::try_from(native.sample_rate)
        .ok()
        .filter(|sample_rate| *sample_rate > 0);
    let channels = u16::try_from(native.ch_layout.nb_channels)
        .ok()
        .filter(|channels| *channels > 0);
    let bit_rate = u64::try_from(native.bit_rate)
        .ok()
        .filter(|bit_rate| *bit_rate > 0);
    let channel_layout = unsafe { av_channel_layout_description(&native.ch_layout) };
    (
        sample_rate,
        channels,
        bit_rate,
        channel_layout,
        native.initial_padding,
        native.trailing_padding,
    )
}

fn opus_head_passthrough_rejection(
    parameters: &ffmpeg::codec::Parameters,
    source_channels: Option<u16>,
) -> Option<&'static str> {
    // SAFETY: Parameters owns a live AVCodecParameters allocation. The
    // extradata is borrowed for the duration of this parse only.
    let native = unsafe { &*parameters.as_ptr() };
    if native.extradata.is_null() || native.extradata_size < 19 {
        return Some("Opus stream does not expose a complete OpusHead");
    }
    // SAFETY: FFmpeg guarantees extradata is readable for extradata_size bytes
    // while the owning Parameters value remains alive.
    let extra =
        unsafe { std::slice::from_raw_parts(native.extradata, native.extradata_size as usize) };
    if !extra.starts_with(b"OpusHead") || extra[8] > 1 {
        return Some("Opus codec initialization data is not a supported OpusHead");
    }
    if !matches!(extra[9], 1 | 2) || extra[18] != 0 {
        return Some("Opus channel mapping configuration is not family zero");
    }
    if source_channels != Some(u16::from(extra[9])) {
        return Some("OpusHead channel count disagrees with stream parameters");
    }
    if u16::from_le_bytes([extra[10], extra[11]]) != 0 {
        return Some("Opus stream has nonzero pre-skip");
    }
    if i16::from_le_bytes([extra[16], extra[17]]) != 0 {
        return Some("OpusHead requires output gain");
    }
    None
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
        .checked_mul(u128::from(OUTPUT_SAMPLE_RATE_HZ))
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
        ffmpeg::ffi::swr_get_delay(context.as_ptr() as *mut _, i64::from(OUTPUT_SAMPLE_RATE_HZ))
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

fn duration_to_sample_position(position: Duration) -> Result<u64, DecoderError> {
    let samples = position.as_nanos() * u128::from(OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    u64::try_from(samples).map_err(|_| {
        DecoderError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
    })
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

fn frame_layout_description(frame: &ffmpeg::frame::Audio) -> Option<String> {
    // SAFETY: `frame` owns this frame and remains alive for the call.
    let frame = unsafe { frame.as_ptr() };
    if frame.is_null() {
        return None;
    }

    // SAFETY: `frame` is a live FFmpeg frame borrowed from the decoder. The
    // native `ch_layout` field is valid for the lifetime of this call.
    unsafe { av_channel_layout_description(&(*frame).ch_layout) }
}

fn layout_description_from_mask(layout: ChannelLayout) -> Option<String> {
    if layout.is_empty() || layout.channels() <= 0 {
        return None;
    }

    // `ChannelLayout` is a legacy bitmask on FFmpeg 6 and an AVChannelLayout
    // wrapper on FFmpeg 7. Convert only through FFmpeg's public mask API; do
    // not assume the Rust wrapper has the native structure's ABI.
    let mut native_layout = std::mem::MaybeUninit::<ffmpeg::ffi::AVChannelLayout>::uninit();
    // SAFETY: FFmpeg initializes `native_layout` on success from the channel
    // mask returned by ffmpeg-next. The output is uninitialized on failure.
    let result = unsafe {
        ffmpeg::ffi::av_channel_layout_from_mask(native_layout.as_mut_ptr(), layout.bits())
    };
    if result < 0 {
        return None;
    }

    // SAFETY: `av_channel_layout_from_mask` returned success and initialized
    // the layout. It is uninitialized after `av_channel_layout_uninit` below.
    let mut native_layout = unsafe { native_layout.assume_init() };
    // SAFETY: the initialized layout remains alive and the buffer is writable
    // for its full length. FFmpeg writes a NUL-terminated description.
    let description = unsafe { av_channel_layout_description(&native_layout) };
    // SAFETY: this layout was initialized by FFmpeg and is no longer used.
    unsafe { ffmpeg::ffi::av_channel_layout_uninit(&mut native_layout) };
    description
}

unsafe fn av_channel_layout_description(layout: &ffmpeg::ffi::AVChannelLayout) -> Option<String> {
    if layout.nb_channels <= 0
        || layout.order == ffmpeg::ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC
    {
        return None;
    }

    let mut buffer = [0_i8; 128];
    // SAFETY: the caller passes a valid AVChannelLayout and `buffer` is
    // writable for the supplied size.
    let result = unsafe {
        ffmpeg::ffi::av_channel_layout_describe(layout, buffer.as_mut_ptr(), buffer.len())
    };
    if result < 0 {
        return None;
    }
    // SAFETY: FFmpeg writes a NUL-terminated description when it succeeds.
    let description = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(description.to_string_lossy().into_owned())
}

#[cfg(test)]
mod packet_reader_tests {
    use super::*;

    #[test]
    fn packet_reader_does_not_initialize_a_pcm_decoder() {
        let mut decoder = AudioDecoder::open_for_playback(
            InputSource::bytes(
                include_bytes!("../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
            ),
            DecoderOptions::default(),
        )
        .unwrap();
        assert!(decoder.decoder.is_none());
        assert!(decoder.read_compressed_packet().unwrap().is_some());
        assert!(decoder.decoder.is_none());
    }
}
