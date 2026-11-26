use super::*;
#[cfg(feature = "experimental-scheduler")]
use std::sync::Arc;

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
    output: Option<ffmpeg::frame::Audio>,
    output_capacity: usize,
    context: Resampler,
    input_format: ffmpeg::format::Sample,
    input_layout: String,
    input_channels: u16,
    input_rate: u32,
}

impl AudioDecoder {
    #[cfg(feature = "experimental-scheduler")]
    pub(crate) fn scheduler_demuxer_supported(&self) -> bool {
        // Conservative containers without nested network inputs or external media references.
        self.reader
            .input
            .name()
            .split(',')
            .all(|name| matches!(name, "ogg" | "mp3" | "aac" | "wav" | "flac"))
    }

    #[cfg(feature = "experimental-scheduler")]
    pub(crate) fn isolate_demux(
        &mut self,
        executor: Arc<crate::playback::blocking::BlockingExecutor>,
    ) -> Option<Arc<demux_input::queued::QueuedInput>> {
        self.reader.input.isolate(executor)
    }

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
        Self::open_for_playback_restricted(source, options, false)
    }

    pub(crate) fn open_for_playback_restricted(
        source: InputSource,
        options: DecoderOptions,
        restricted: bool,
    ) -> Result<Self, DecoderError> {
        let reader = AudioPacketReader::open(source, options, restricted)?;
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

    /// The next demuxed packet is the stream's first, so it holds any pre-skip region.
    pub(crate) fn at_first_packet(&self) -> bool {
        self.reader.first_packet_pending && self.reader.pending_packets.is_empty()
    }

    /// The Opus pre-skip, in 48 kHz samples, of a stream that can use packet passthrough.
    pub(crate) fn opus_pre_skip(&self) -> u32 {
        self.reader.pre_skip
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
        // The next unread packet is this one again, so the source position is back at its start.
        if packet.source_start_sample.is_some() {
            self.reader.next_source_sample = packet.source_start_sample;
        }
        self.reader.pending_packets.push_front(packet);
        self.reader.demux_finished = false;
        self.finished = false;
    }

    /// Decode the next packet and return only its first `head_samples` of PCM, then put the packet
    /// back so it can be forwarded unchanged. Used to feed the Opus encoder the samples that
    /// drain its lookahead before a switch back to source packets.
    ///
    /// The decoder must be idle (no pending frames) and is left holding state that already
    /// includes the packet, so the caller must leave PCM decoding immediately afterwards.
    /// Returns `Ok(None)` at end of input or when the packet cannot be restored or is shorter than
    /// `head_samples`; in that case nothing has been consumed from the demuxer.
    pub(crate) fn overshoot_next_packet(
        &mut self,
        head_samples: usize,
    ) -> Result<Option<DecodedPcmFrame>, DecoderError> {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            match self.poll_overshoot_next_packet(head_samples, &mut cx) {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => self.reader.input.wait_ready(),
            }
        }
    }

    pub(crate) fn poll_overshoot_next_packet(
        &mut self,
        head_samples: usize,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<DecodedPcmFrame>, DecoderError>> {
        let _log_capture = crate::ffmpeg_logging::capture();
        self.check_cancelled()?;
        if self.decoder.is_none() || !self.pending_frames.is_empty() {
            return std::task::Poll::Ready(Ok(None));
        }
        let Some(packet) = std::task::ready!(self.poll_compressed_packet(cx))? else {
            return std::task::Poll::Ready(Ok(None));
        };
        if packet.has_trimming_metadata
            || !packet
                .samples_per_channel
                .is_some_and(|samples| samples as usize >= head_samples)
        {
            self.queue_compressed_packet(packet);
            return std::task::Poll::Ready(Ok(None));
        }
        self.send_packet(&packet.packet)?;
        self.drain_decoder()?;
        let head = self
            .pending_frames
            .pop_front()
            .filter(|frame| frame.frame.samples_per_channel >= head_samples);
        self.pending_frames.clear();
        let head = head.map(|mut frame| {
            frame
                .frame
                .samples
                .truncate(head_samples * usize::from(OUTPUT_CHANNELS));
            frame.frame.samples_per_channel = head_samples;
            frame
        });
        match head {
            Some(frame) => {
                self.queue_compressed_packet(packet);
                std::task::Poll::Ready(Ok(Some(frame)))
            }
            None => {
                self.queue_compressed_packet(packet);
                std::task::Poll::Ready(Err(DecoderError::Decode(
                    "packet decoded to fewer samples than the encoder lookahead".into(),
                )))
            }
        }
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

    /// Decode and return the next normalized PCM block, or `Ok(None)` after
    /// the decoder has drained all codec and resampler delay.
    pub fn next_frame(&mut self) -> Result<Option<PcmFrame>, DecoderError> {
        Ok(self.next_timed_frame()?.map(|frame| frame.frame))
    }

    pub(crate) fn next_timed_frame(&mut self) -> Result<Option<DecodedPcmFrame>, DecoderError> {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            match self.poll_next_timed_frame(&mut cx) {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => self.reader.input.wait_ready(),
            }
        }
    }

    pub(crate) fn poll_compressed_packet(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<DemuxedAudioPacket>, DecoderError>> {
        self.check_cancelled()?;
        self.reader.poll_audio_packet(cx)
    }

    pub(crate) fn poll_next_timed_frame(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<DecodedPcmFrame>, DecoderError>> {
        let _log_capture = crate::ffmpeg_logging::capture();
        self.check_cancelled()?;
        if self.decoder.is_none() {
            self.activate_decoder(&[], None)?;
        }
        if let Some(frame) = self.pending_frames.pop_front() {
            return std::task::Poll::Ready(Ok(Some(frame)));
        }
        if self.finished {
            return std::task::Poll::Ready(Ok(None));
        }

        loop {
            self.check_cancelled()?;

            if !self.reader.demux_finished {
                match std::task::ready!(self.poll_compressed_packet(cx))? {
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
                return std::task::Poll::Ready(Ok(Some(frame)));
            }

            if self.reader.demux_finished {
                self.flush_resampler()?;
                self.finished = true;
                return std::task::Poll::Ready(Ok(self.pending_frames.pop_front()));
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

mod activation;

mod seeking;

mod resampling;
