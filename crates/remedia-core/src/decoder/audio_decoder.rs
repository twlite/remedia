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
