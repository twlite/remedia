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
    /// The stream's Opus pre-skip in 48 kHz samples; zero when it has none.
    pub(super) pre_skip: u32,
    /// The next packet is the first of the stream, so it carries the pre-skip region.
    pub(super) first_packet_pending: bool,
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
    /// Decoder pre-roll samples at the start of this packet that are not media. The packet is
    /// forwarded unchanged, so these samples are played, but they are verified to be encoder
    /// delay and are excluded from the media timeline. Only the stream's first packet can have any.
    pub leading_skip: u32,
    /// Pre-roll samples at the start of this packet that are not media, whether the packet is
    /// forwarded (`leading_skip`) or decoded and trimmed. The media it covers starts after them.
    pub media_offset: u32,
}

/// Pre-roll up to this absolute level (about -60 dBFS) is always treated as encoder delay.
const PREROLL_QUIET_PEAK: f32 = 0.001;

/// Pre-roll may reach this fraction of the packet's following audio and still be encoder delay.
///
/// libopus delays its input by its lookahead, so the first `pre_skip` decoded samples are silence
/// plus, for an abrupt start, the transform's pre-echo. Measured on real files that pre-echo
/// peaks between -30 dB and -20 dB relative to the audio that follows (a full-level burst at the
/// first sample is the worst case), and Remedia's own PCM encoder emits the same pre-echo. A
/// stream whose "pre-roll" is as loud as the audio after it was mislabeled, and trimming it
/// would change what is heard.
const PREROLL_RELATIVE_PEAK: f32 = 0.35;

/// Decode `packet` with a fresh libopus decoder and report whether its first `pre_skip` output
/// samples look like encoder delay, the only content a player is entitled to discard. `None` if
/// the packet cannot be decoded.
fn preroll_is_encoder_delay(packet: &[u8], pre_skip: u32) -> Option<bool> {
    let mut decoder = opus2::Decoder::new(OUTPUT_SAMPLE_RATE_HZ, opus2::Channels::Stereo).ok()?;
    // 120 ms is the longest Opus packet.
    let mut pcm = vec![0.0_f32; 5_760 * 2];
    let samples = decoder.decode_float(packet, &mut pcm, false).ok()?;
    let split = (pre_skip as usize).min(samples) * 2;
    let peak = |block: &[f32]| {
        block
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
    };
    let (preroll, media) = (peak(&pcm[..split]), peak(&pcm[split..samples * 2]));
    Some(preroll <= PREROLL_QUIET_PEAK.max(media * PREROLL_RELATIVE_PEAK))
}

/// Read the `(skip_samples, discard_padding)` pair FFmpeg attaches to a trimmed packet.
fn skip_samples(packet: &ffmpeg::Packet) -> (u32, u32) {
    packet
        .side_data()
        .find(|side_data| side_data.kind() == ffmpeg::codec::packet::side_data::Type::SkipSamples)
        .and_then(|side_data| {
            let data = side_data.data();
            Some((
                u32::from_le_bytes(data.get(0..4)?.try_into().ok()?),
                u32::from_le_bytes(data.get(4..8)?.try_into().ok()?),
            ))
        })
        .unwrap_or((0, 0))
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
        let mut pre_skip = 0;
        let passthrough_rejection = if codec_id != ffmpeg::codec::Id::OPUS {
            Some("selected codec is not Opus")
        } else if sample_rate != Some(OUTPUT_SAMPLE_RATE_HZ) {
            Some("Opus stream sample rate is not 48 kHz")
        } else if !matches!(channels, Some(1 | 2)) {
            Some("Opus channel layout is not mono or stereo")
        } else if trailing_padding != 0 {
            Some("stream requires trailing sample trimming")
        } else {
            match opus_head_passthrough(&codec_parameters, channels) {
                Err(reason) => Some(reason),
                // The container and the OpusHead must agree, or the real skip is unknown.
                Ok(head_skip) if initial_padding != i32::from(head_skip) => {
                    Some("codec initial padding disagrees with the OpusHead pre-skip")
                }
                // Only a pre-skip inside the first 20 ms packet can be modeled by that packet.
                Ok(head_skip) if head_skip >= 960 => {
                    Some("Opus pre-skip spans more than one packet")
                }
                Ok(head_skip) => {
                    pre_skip = u32::from(head_skip);
                    None
                }
            }
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
            pre_skip,
            first_packet_pending: true,
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
            // A packet put back by `queue_compressed_packet` moved the position back to its start.
            if let (Some(start), Some(samples)) =
                (packet.source_start_sample, packet.samples_per_channel)
            {
                self.next_source_sample = Some(
                    start.saturating_add(u64::from(samples.saturating_sub(packet.media_offset))),
                );
            }
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
                    let (start_skip, end_skip) = skip_samples(&packet);
                    let is_first = std::mem::take(&mut self.first_packet_pending);
                    let carries_pre_skip = is_first && self.pre_skip > 0;
                    // FFmpeg's Ogg demuxer attaches the pre-skip to the first packet; Matroska
                    // exposes it only through the codec parameters.
                    let skip_matches_stream = carries_pre_skip
                        && (start_skip == 0 || start_skip == self.pre_skip)
                        && end_skip == 0
                        && samples_per_channel.is_some_and(|samples| samples > self.pre_skip);
                    let mut leading_skip = 0;
                    if skip_matches_stream {
                        let delay = packet
                            .data()
                            .and_then(|bytes| preroll_is_encoder_delay(bytes, self.pre_skip));
                        // Anything else is decoded, where FFmpeg trims the pre-skip itself.
                        if delay == Some(true) {
                            leading_skip = self.pre_skip;
                        }
                    }
                    let unmodeled_skip = (start_skip != 0 && leading_skip == 0)
                        || end_skip != 0
                        || (carries_pre_skip && leading_skip == 0);
                    let has_skip_samples = unmodeled_skip;
                    let has_trimmed_duration = packet.duration() > 0
                        && samples_per_channel.is_some_and(|samples| {
                            timestamp_to_output_sample(
                                Some(packet.duration()),
                                self.stream_time_base,
                                OUTPUT_SAMPLE_RATE_HZ,
                            ) != Some(u64::from(samples))
                        });
                    // A packet's media starts after its pre-skip region, at media sample 0. FFmpeg
                    // also reports that region when a seek lands on the first packet again.
                    let preskip_media_offset = if carries_pre_skip && skip_matches_stream {
                        self.pre_skip
                    } else if timestamp_sample.is_none()
                        && start_skip != 0
                        && samples_per_channel.is_some_and(|samples| start_skip < samples)
                    {
                        start_skip
                    } else {
                        0
                    };
                    let source_start_sample = if preskip_media_offset != 0 {
                        Some(0)
                    } else {
                        source_start_sample
                    };
                    if let (Some(start), Some(samples)) = (source_start_sample, samples_per_channel)
                    {
                        let media_samples = samples - preskip_media_offset;
                        self.next_source_sample =
                            Some(start.saturating_add(u64::from(media_samples)));
                    }
                    let demuxed = DemuxedAudioPacket {
                        packet,
                        source_start_sample,
                        source_position_estimated: timestamp_sample.is_none()
                            && preskip_media_offset == 0,
                        samples_per_channel,
                        has_trimming_metadata: has_skip_samples || has_trimmed_duration,
                        leading_skip,
                        media_offset: preskip_media_offset,
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
