use super::*;

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

    /// Seek to a media position. Seekable inputs use FFmpeg's demuxer, while
    /// one-shot byte streams can only move forward by decoding and discarding
    /// audio until the requested position. Backward byte-stream requests and
    /// demuxers that cannot seek return [`DecoderError::UnsupportedSeeking`].
    pub fn seek(&mut self, position: Duration) -> Result<(), DecoderError> {
        self.seek_with_pending(position, &mut None)
    }

    /// A worker may already hold one decoded frame that has not been submitted
    /// to its output. Preserve it while walking a one-shot byte stream forward.
    pub(crate) fn seek_with_pending(
        &mut self,
        position: Duration,
        pending_frame: &mut Option<DecodedPcmFrame>,
    ) -> Result<(), DecoderError> {
        let _log_capture = crate::ffmpeg_logging::capture();
        self.check_cancelled()?;
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
        if !self.reader.input.is_seekable() {
            return self.seek_forward(target_sample, position, pending_frame);
        }
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
        *pending_frame = None;
        Ok(())
    }

    /// Consume the source in order without retaining the skipped PCM. This is
    /// intentionally a *media* seek, not an AVIO byte offset seek: compressed
    /// media can have variable bitrates and codec-dependent timestamps.
    fn seek_forward(
        &mut self,
        target_sample: u64,
        target: Duration,
        first_frame: &mut Option<DecodedPcmFrame>,
    ) -> Result<(), DecoderError> {
        let first_available = first_frame
            .as_ref()
            .or_else(|| self.pending_frames.front())
            .map(|frame| {
                frame
                    .source_start_sample
                    .unwrap_or(frame.frame.start_sample)
            })
            .or(self.next_sample_position)
            .or_else(|| self.reader.source_boundary_sample());
        if first_available.is_some_and(|available| target_sample < available) {
            return Err(DecoderError::UnsupportedSeeking(
                "cannot seek backwards in a one-shot byte stream".to_owned(),
            ));
        }

        loop {
            self.check_cancelled()?;
            let next = match first_frame.take() {
                Some(frame) => Some(frame),
                None => self.next_timed_frame()?,
            };
            let Some(mut frame) = next else {
                let final_sample = self.next_sample_position.unwrap_or(0);
                return Err(DecoderError::SeekOutOfRange {
                    target,
                    duration: Duration::from_millis(
                        final_sample.saturating_mul(1_000) / u64::from(OUTPUT_SAMPLE_RATE_HZ),
                    ),
                });
            };

            let start = frame
                .source_start_sample
                .unwrap_or(frame.frame.start_sample);
            let end = start.saturating_add(frame.frame.samples_per_channel as u64);
            if end <= target_sample {
                continue;
            }

            let skip = target_sample.saturating_sub(start) as usize;
            if skip != 0 {
                frame
                    .frame
                    .samples
                    .drain(..skip * usize::from(OUTPUT_CHANNELS));
                frame.frame.samples_per_channel -= skip;
            }

            // FFmpeg timestamps and the decoder's running sample index may
            // differ (notably for MP3 encoder delay). Rebase the retained PCM
            // and any already decoded frames onto the source timeline.
            let retained_start = start.saturating_add(skip as u64);
            frame.frame.start_sample = retained_start;
            frame.source_start_sample = Some(retained_start);
            let mut next_sample =
                retained_start.saturating_add(frame.frame.samples_per_channel as u64);
            for buffered in &mut self.pending_frames {
                buffered.frame.start_sample = next_sample;
                next_sample = next_sample.saturating_add(buffered.frame.samples_per_channel as u64);
            }
            self.next_sample_position = Some(next_sample);

            // Put the first retained frame back for the regular playback loop.
            // Decoder state and the bounded byte-stream queue stay intact.
            self.pending_frames.push_front(frame);
            return Ok(());
        }
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

#[cfg(test)]
mod packet_reader_tests {
    use super::*;

    #[test]
    fn packet_reader_does_not_initialize_a_pcm_decoder() {
        let mut decoder = AudioDecoder::open_for_playback(
            InputSource::bytes(
                include_bytes!("../../tests/fixtures/generated-opus-passthrough.ogg").to_vec(),
            ),
            DecoderOptions::default(),
        )
        .unwrap();
        assert!(decoder.decoder.is_none());
        assert!(decoder.read_compressed_packet().unwrap().is_some());
        assert!(decoder.decoder.is_none());
    }
}
