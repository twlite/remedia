use super::*;

/// Owns the one FFmpeg demux context and selected stream. It can read
/// compressed audio packets without opening a codec; PCM decoding is attached
/// lazily when a consumer needs it.
pub(crate) struct AudioPacketReader {
    pub(super) input: OpenedInput,
    pub(super) codec_parameters: ffmpeg::codec::Parameters,
    pub(super) audio_stream_index: usize,
    pub(super) network_input: bool,
    pub(super) stream_time_base: Rational,
    pub(super) info: AudioStreamInfo,
    pub(super) cancellation: CancellationToken,
    pub(super) pending_packets: VecDeque<DemuxedAudioPacket>,
    pub(super) next_source_sample: Option<u64>,
    pub(super) stream_start_sample: Option<u64>,
    pub(super) demux_finished: bool,
    pub(super) passthrough_rejection: Option<&'static str>,
    pub(super) packet_history: VecDeque<DemuxedAudioPacket>,
    pub(super) packet_history_samples: u64,
    pub(super) packet_history_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct DemuxedAudioPacket {
    pub packet: ffmpeg::Packet,
    pub source_start_sample: Option<u64>,
    pub source_position_estimated: bool,
    pub samples_per_channel: Option<u32>,
    pub has_trimming_metadata: bool,
}

impl AudioPacketReader {
    pub(super) fn open(source: InputSource, options: DecoderOptions) -> Result<Self, DecoderError> {
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
        let stream_start_time = stream.start_time();
        let parameters = stream.parameters();
        let codec_id = parameters.id();
        let codec_parameters = parameters.clone();
        let (sample_rate, channels, _, _, initial_padding, trailing_padding) =
            codec_parameter_metadata(&codec_parameters);
        let info = inspect_audio(&input)?;
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

    pub(super) fn opus_passthrough_rejection(&self) -> Option<&'static str> {
        self.passthrough_rejection
    }

    pub(super) fn source_boundary_sample(&self) -> Option<u64> {
        self.next_source_sample
    }

    pub(super) fn history_before(&self, boundary_sample: u64) -> Vec<DemuxedAudioPacket> {
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

    pub(super) fn clear_packet_history(&mut self) {
        self.packet_history.clear();
        self.packet_history_samples = 0;
        self.packet_history_bytes = 0;
    }

    pub(super) fn remember_packet(&mut self, packet: &DemuxedAudioPacket) {
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

    pub(super) fn read_audio_packet(&mut self) -> Result<Option<DemuxedAudioPacket>, DecoderError> {
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
