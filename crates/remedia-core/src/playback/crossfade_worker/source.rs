//! Ownership and reading of direct or promoted prepared inputs.

use super::*;

/// Either the original FFmpeg decoder, or a prebuffered producer that has been
/// promoted without opening another network stream or restarting the encoder.
enum ActiveInput {
    Direct {
        decoder: Box<AudioDecoder>,
        pcm: VecDeque<f32>,
        ended: bool,
    },
    Prepared(Arc<PreparedStage>),
}

pub(super) struct ActiveSource {
    input: ActiveInput,
    encoded: bool,
    eligible: bool,
    boundary: Option<u64>,
    pub(super) consumed: u64,
    pub(super) duration_samples: Option<u64>,
}

impl ActiveSource {
    pub(super) fn decoder(decoder: AudioDecoder) -> Self {
        let eligible = decoder.opus_passthrough_rejection().is_none();
        let duration_samples = decoder.info().duration.map(samples_from_duration);
        Self {
            input: ActiveInput::Direct {
                decoder: Box::new(decoder),
                pcm: VecDeque::new(),
                ended: false,
            },
            encoded: false,
            eligible,
            boundary: None,
            consumed: 0,
            duration_samples,
        }
    }

    pub(super) fn prepared(
        stage: Arc<PreparedStage>,
        consumed: u64,
        info: &AudioStreamInfo,
    ) -> Self {
        Self {
            input: ActiveInput::Prepared(stage),
            encoded: false,
            eligible: false,
            boundary: None,
            consumed,
            duration_samples: info.duration.map(samples_from_duration),
        }
    }

    pub(super) fn enter_pcm(&mut self) -> Result<(), PlaybackError> {
        if let ActiveInput::Direct { decoder, .. } = &mut self.input {
            if self.encoded {
                let boundary = self.boundary.unwrap_or(self.consumed);
                let history = decoder.packet_history_before(boundary);
                decoder.activate_decoder(&history, Some(boundary))?;
            } else {
                decoder.activate_decoder(&[], None)?;
            }
        }
        self.encoded = false;
        Ok(())
    }

    /// Submit a whole source packet only when it fits before the overlap.
    /// Prepared producers retain PCM ownership and never reopen their input.
    pub(super) fn try_encoded(
        &mut self,
        shared: &SessionShared,
        output: &Arc<dyn MediaOutput>,
        processor: &mut PcmProcessor,
        cancellation: &CancellationToken,
        before_overlap: Option<u64>,
    ) -> Result<Option<bool>, PlaybackError> {
        let wants_encoded = {
            let state = lock(&shared.data);
            state.volume == 1.0 && state.filter_graph.is_none() && processor.expression().is_none()
        };
        if !self.eligible || !wants_encoded || before_overlap == Some(0) {
            return Ok(None);
        }
        let Some(encoded_output) = output.encoded_opus_output() else {
            return Ok(None);
        };
        let ActiveInput::Direct {
            decoder,
            pcm,
            ended,
        } = &mut self.input
        else {
            return Ok(None);
        };
        if !self.encoded {
            // A return from PCM requires both source and encoder packet boundaries.
            // Conservatively keep transcoding if any decoded/resampled audio is pending.
            if !pcm.is_empty()
                || decoder.has_pending_frames()
                || !processor.next_sample().is_multiple_of(960)
            {
                return Ok(None);
            }
            if self.consumed > 0 && decoder.source_boundary_sample() != self.boundary {
                return Ok(None);
            }
            decoder.deactivate_decoder()?;
            encoded_output.prepare_passthrough(processor.next_sample(), cancellation)?;
            self.encoded = true;
        }
        if *ended {
            return Ok(Some(false));
        }
        let Some(packet) = decoder.read_compressed_packet()? else {
            *ended = true;
            return Ok(Some(false));
        };
        let encoded = encoded_packet_from_demuxed(&packet, decoder.info().channels);
        let still_wanted = {
            let state = lock(&shared.data);
            state.volume == 1.0 && state.filter_graph.is_none() && state.pending_seek.is_none()
        };
        let fits = encoded.as_ref().is_some_and(|encoded| {
            !packet.has_trimming_metadata
                && encoded_output.supports_encoded_packet(encoded)
                && before_overlap
                    .is_none_or(|left| u64::from(encoded.media_samples_per_channel) <= left)
        });
        if !fits || !still_wanted {
            decoder.queue_compressed_packet(packet);
            if !fits {
                self.eligible = false;
            }
            return Ok(None);
        }
        let mut encoded = encoded.expect("checked packet eligibility");
        encoded.start_sample = processor
            .record_passthrough_packet(
                encoded.samples_per_channel,
                packet.source_start_sample,
                packet.source_position_estimated,
            )
            .map_err(|error| PlaybackError::FilterGraph(Arc::new(error)))?;
        let packet_samples = encoded.media_samples_per_channel;
        {
            let mut state = lock(&shared.data);
            state.processing_mode = Some(PlaybackProcessingMode::Passthrough);
            shared.changed.notify_all();
        }
        encoded_output.write_encoded_packet(encoded, cancellation)?;
        self.consumed += u64::from(packet_samples);
        self.boundary = packet
            .source_start_sample
            .map(|sample| sample + u64::from(packet_samples));
        Ok(Some(true))
    }

    pub(super) fn passthrough_eligible(&self) -> bool {
        self.eligible
    }

    pub(super) fn encoded_mode(&self) -> bool {
        self.encoded
    }

    pub(super) fn read(
        &mut self,
        wanted: usize,
        cancellation: &CancellationToken,
    ) -> Result<Option<Vec<f32>>, PlaybackError> {
        if wanted == 0 {
            return Ok(None);
        }
        self.enter_pcm()?;
        let samples = match &mut self.input {
            ActiveInput::Direct {
                decoder,
                pcm,
                ended,
            } => {
                while pcm.len() < wanted * 2 && !*ended && !cancellation.is_cancelled() {
                    match decoder.next_frame()? {
                        Some(frame) => pcm.extend(frame.samples),
                        None => *ended = true,
                    }
                }
                if pcm.is_empty() {
                    return Ok(None);
                }
                pcm.drain(..(wanted * 2).min(pcm.len())).collect()
            }
            ActiveInput::Prepared(stage) => {
                match stage.buffer.read_for_playback(wanted, cancellation) {
                    Some(frame) => frame,
                    None if cancellation.is_cancelled() => return Err(PlaybackError::Cancelled),
                    None if stage.snapshot().state == StageState::Failed => {
                        return Err(PlaybackError::UnsupportedSeeking(
                            "prepared source failed while decoding".into(),
                        ));
                    }
                    None => return Ok(None),
                }
            }
        };
        self.consumed += (samples.len() / 2) as u64;
        if self.eligible {
            self.boundary = Some(self.consumed);
        }
        Ok(Some(samples))
    }

    pub(super) fn seek(
        &mut self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), PlaybackError> {
        match &mut self.input {
            ActiveInput::Direct {
                decoder,
                pcm,
                ended,
            } => {
                decoder.activate_decoder(&[], None)?;
                decoder.seek(position)?;
                self.encoded = false;
                self.eligible = false;
                pcm.clear();
                *ended = false;
            }
            ActiveInput::Prepared(stage) => {
                let source = stage.source.clone();
                if !source.can_seek() {
                    return Err(PlaybackError::UnsupportedSeeking(
                        "prepared byte stream cannot rewind".into(),
                    ));
                }
                let mut decoder = AudioDecoder::open_with_options(
                    source,
                    DecoderOptions {
                        cancellation: cancellation.clone(),
                        ..DecoderOptions::default()
                    },
                )?;
                decoder.activate_decoder(&[], None)?;
                decoder.seek(position)?;
                self.encoded = false;
                self.eligible = false;
                stage.cancel_and_join();
                self.input = ActiveInput::Direct {
                    decoder: Box::new(decoder),
                    pcm: VecDeque::new(),
                    ended: false,
                };
            }
        };
        self.eligible = false;
        self.encoded = false;
        self.consumed = samples_from_duration(position);
        Ok(())
    }
}

impl Drop for ActiveSource {
    fn drop(&mut self) {
        if let ActiveInput::Prepared(stage) = &self.input {
            stage.cancel_and_join();
        }
    }
}
