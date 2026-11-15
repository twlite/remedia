//! Ownership and reading of direct or promoted prepared inputs.

use super::*;
use crate::decoder::{OPUS_ENCODER_LOOKAHEAD, OPUS_RECOVERY_WARM_FRAMES};

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
    source: InputSource,
    /// How to obtain this track again, for a seek the active decoder cannot perform.
    reopen: fallback::Reopen,
    probe: Option<super::tail::TailProbe>,
    effective_end: Option<u64>,
    encoded: bool,
    eligible: bool,
    /// The stream could use source packets if the current position and output allow it. Unlike
    /// `eligible`, this survives a seek and a packet that merely would not fit before an overlap.
    stream_eligible: bool,
    boundary: Option<u64>,
    /// The stream's Opus pre-skip: its packets begin where `(media sample + pre_skip) % 960 == 0`.
    pre_skip: u32,
    /// Samples fed to the Opus encoder since it last started from an empty state.
    pcm_run: u64,
    /// Samples held in the output encoder's partial 20 ms frame, when known. A return to source
    /// packets needs it to be the 648 samples that the next packet's first 312 complete.
    encoder_phase: Option<u64>,
    /// The first samples of the next packet are queued in `pcm` to drain the encoder lookahead.
    junction: bool,
    /// Warm-up audio (what the client just heard) to prime the output encoder with.
    prime: Option<Vec<f32>>,
    /// Packets are accounted in heard media since the encoder was primed on forwarded audio.
    truthful: bool,
    /// Keeps a promoted source's cancellation linked to the session after its decoder was adopted.
    _cancel_link: Option<Arc<dyn crate::source::CancellationWake>>,
    pub(super) consumed: u64,
    pub(super) duration_samples: Option<u64>,
}

impl ActiveSource {
    pub(super) fn decoder(decoder: AudioDecoder, source: InputSource) -> Self {
        let eligible = decoder.opus_passthrough_rejection().is_none();
        let pre_skip = decoder.opus_pre_skip();
        let duration_samples = decoder.info().duration.map(samples_from_duration);
        Self {
            reopen: fallback::Reopen::new(&source, None),
            source,
            probe: None,
            effective_end: None,
            input: ActiveInput::Direct {
                decoder: Box::new(decoder),
                pcm: VecDeque::new(),
                ended: false,
            },
            encoded: false,
            eligible,
            stream_eligible: eligible,
            boundary: None,
            pre_skip,
            pcm_run: 0,
            encoder_phase: Some(0),
            junction: false,
            prime: None,
            truthful: false,
            _cancel_link: None,
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
            source: stage.source.clone(),
            reopen: stage.reopen.clone(),
            probe: None,
            effective_end: None,
            input: ActiveInput::Prepared(stage),
            encoded: false,
            eligible: false,
            stream_eligible: false,
            boundary: None,
            pre_skip: 0,
            pcm_run: 0,
            encoder_phase: Some(0),
            junction: false,
            prime: None,
            truthful: false,
            _cancel_link: None,
            consumed,
            duration_samples: info.duration.map(samples_from_duration),
        }
    }

    /// Use `reopen` to obtain this track again, in place of the one derived from its source.
    pub(super) fn with_reopen(mut self, reopen: fallback::Reopen) -> Self {
        self.reopen = reopen;
        self
    }

    pub(super) fn reopen(&self) -> &fallback::Reopen {
        &self.reopen
    }

    /// Whether the active input can reposition itself without help. A promoted source's seek
    /// opens a fresh decoder, which can reposition itself exactly when the staged one could.
    pub(super) fn can_seek_in_place(&self) -> bool {
        match &self.input {
            ActiveInput::Direct { decoder, .. } => decoder.can_seek_in_place(),
            ActiveInput::Prepared(stage) => stage.random_access(),
        }
    }

    /// The Opus encoder's partial frame at the start of a block of `block` samples that was just
    /// read, when it is known.
    pub(super) fn encoder_phase_before(&self, block: usize) -> Option<u64> {
        self.encoder_phase
            .map(|phase| (phase + 960 - (block as u64 % 960)) % 960)
    }

    /// Whether a replacement decoder's packets could follow this source's re-encoded audio: the
    /// output encodes Opus, the stream is a plain Opus file whose pre-skip equals the encoder's
    /// lookahead, and the encoder's frame phase is known.
    pub(super) fn can_recover_packets(
        &self,
        output: &Arc<dyn MediaOutput>,
        replacement: &AudioDecoder,
    ) -> bool {
        output.encoded_opus_output().is_some()
            && self.encoder_phase.is_some()
            && replacement.opus_passthrough_rejection().is_none()
            && replacement.opus_pre_skip() == OPUS_ENCODER_LOOKAHEAD
    }

    /// Continue this track on `decoder`, a second decoder on the same track that has already
    /// produced the audio up to `consumed`. `queued` is audio it decoded beyond that point.
    ///
    /// The output encoder is not touched: its state follows the audio, which does not change.
    /// `block` is the length of the audio just handed to the output from the replacement, and
    /// `position` the source sample it began at.
    ///
    /// With `recover`, the source may return to forwarding packets: it counts the encoder's warm-up
    /// from this block, so the client's decoder has settled on the new audio before packets resume.
    pub(super) fn adopt_replacement(
        &mut self,
        decoder: Box<AudioDecoder>,
        queued: VecDeque<f32>,
        info: &AudioStreamInfo,
        position: u64,
        block: usize,
        recover: bool,
    ) {
        if let ActiveInput::Prepared(stage) = &self.input {
            stage.cancel_and_join();
        }

        self.pre_skip = decoder.opus_pre_skip();
        self.stream_eligible = recover;
        self.eligible = recover;
        self.input = ActiveInput::Direct {
            decoder,
            pcm: queued,
            ended: false,
        };
        self.encoded = false;
        self.boundary = None;
        self.junction = false;
        self.prime = None;
        self.probe = None;
        self.effective_end = None;
        self._cancel_link = None;
        self.duration_samples = info.duration.map(samples_from_duration);
        // The new audio is the block just read; the audio before it belonged to the old source.
        self.consumed = position + block as u64;
        self.pcm_run = block as u64;
        if recover {
            self.boundary = Some(self.consumed);
        }
    }

    /// Take over a promoted stage's decoder once its staged PCM is fully consumed, so a source
    /// that can use encoded Opus packets stops decoding on a separate thread. Returns whether the
    /// decoder is now owned by this source.
    fn adopt_handed_off_decoder(&mut self) -> bool {
        let ActiveInput::Prepared(stage) = &self.input else {
            return false;
        };
        if !stage.handoff_complete() {
            return false;
        }
        let Some(decoder) = stage.take_handed_off_decoder() else {
            return false;
        };
        let stage = Arc::clone(stage);
        stage.join_finished_worker();
        self.stream_eligible = decoder.opus_passthrough_rejection().is_none();
        self.pre_skip = decoder.opus_pre_skip();
        self.eligible = self.stream_eligible;
        self.boundary = Some(self.consumed);
        self.encoded = false;
        self._cancel_link = stage.cancel_link();
        self.input = ActiveInput::Direct {
            decoder,
            pcm: VecDeque::new(),
            ended: false,
        };
        true
    }

    /// Record the encoder state a promoted source inherits from the overlap that just ended: how
    /// long the encoder has been running and how many samples its partial frame holds.
    pub(super) fn inherit_encoder_state(
        &mut self,
        pcm_run: u64,
        phase: Option<u64>,
        truthful: bool,
    ) {
        self.pcm_run = pcm_run;
        self.encoder_phase = phase;
        self.truthful = truthful;
    }

    /// The encoder phase if this source were to start PCM now: empty after forwarded packets.
    pub(super) fn encoder_phase_now(&self) -> Option<u64> {
        if self.encoded {
            Some(0)
        } else {
            self.encoder_phase
        }
    }

    /// The output encoder was just flushed at `sample` and aligned to the 960-sample media grid.
    pub(super) fn mark_encoder_aligned(&mut self, sample: u64) {
        self.encoder_phase = Some(sample % 960);
        self.pcm_run = 0;
    }

    /// A time-changing filter fed the encoder an unknown number of samples.
    pub(super) fn forget_encoder_phase(&mut self) {
        self.encoder_phase = None;
    }

    /// Samples the next PCM read should request so reads end on the source packet grid. After a
    /// seek the first read is shorter; in steady state this is one full frame.
    pub(super) fn grid_aligned_read(&self, frame: usize) -> usize {
        if self.stream_eligible && matches!(self.input, ActiveInput::Direct { .. }) {
            frame - ((self.consumed + u64::from(self.pre_skip)) % frame as u64) as usize
        } else {
            frame
        }
    }

    /// Whether this source may return to source packets, for aligning a seek's output flush.
    pub(super) fn may_resume_encoded(&self) -> bool {
        self.eligible
    }

    pub(super) fn scheduling_end(&mut self, staged: bool, fade: u64) -> Option<u64> {
        if staged
            && self.probe.is_none()
            && matches!(self.source, InputSource::File(_) | InputSource::Bytes(_))
            && let Some(duration) = self.duration_samples
        {
            self.probe = Some(super::tail::TailProbe::start(
                self.source.clone(),
                duration,
                fade,
            ));
        }
        if self.effective_end.is_none()
            && let Some(end) = self.probe.as_ref().and_then(|probe| probe.endpoint())
            && end.saturating_sub(fade) >= self.consumed
        {
            self.effective_end = Some(end);
        }
        self.effective_end.or(self.duration_samples)
    }

    pub(super) fn enter_pcm(&mut self) -> Result<(), PlaybackError> {
        if let ActiveInput::Direct { decoder, .. } = &mut self.input {
            if self.encoded {
                let boundary = self.boundary.unwrap_or(self.consumed);
                let history = decoder.packet_history_before(boundary);
                if self.pre_skip == OPUS_ENCODER_LOOKAHEAD {
                    // Keep the warm-up audio: it is what the client just heard, and the output
                    // encoder is primed with it so the switch has no gap and no cold start.
                    let (pcm, start) = decoder.activate_decoder_capturing(&history, boundary)?;
                    let length = (pcm.len() / 2) as u64;
                    self.prime = (start.is_some()
                        && length <= boundary
                        && (boundary - length).is_multiple_of(960))
                    .then_some(pcm);
                } else {
                    decoder.activate_decoder(&history, Some(boundary))?;
                }
            } else {
                decoder.activate_decoder(&[], None)?;
            }
        }
        if self.encoded {
            // The output encoder restarts from an empty state on a source packet boundary.
            self.pcm_run = 0;
            self.encoder_phase = Some(0);
            self.truthful = false;
        }
        self.encoded = false;
        Ok(())
    }

    /// Enter PCM from forwarded packets and, when possible, prime the output encoder with the
    /// audio the client just heard, so there is no gap and no cold encoder at the switch.
    pub(super) fn enter_pcm_seamless(
        &mut self,
        output: &Arc<dyn MediaOutput>,
        live_start: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), PlaybackError> {
        self.enter_pcm()?;
        let Some(pcm) = self.prime.take() else {
            return Ok(());
        };
        if let Some(encoded) = output.encoded_opus_output()
            && encoded.begin_seamless_pcm(&pcm, live_start, cancellation)?
        {
            let length = (pcm.len() / 2) as u64;
            self.encoder_phase = Some(length % 960);
            self.pcm_run = length;
            self.truthful = true;
        }
        Ok(())
    }

    pub(super) fn truthful(&self) -> bool {
        self.truthful
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
        if !wants_encoded || before_overlap == Some(0) {
            return Ok(None);
        }
        let Some(encoded_output) = output.encoded_opus_output() else {
            return Ok(None);
        };
        if let ActiveInput::Prepared(stage) = &self.input {
            // A promoted Opus source keeps decoding on its stage thread. Ask it to park its
            // decoder at the next frame boundary; this source adopts it after draining the
            // staged PCM, then the checks below decide whether packets can resume.
            if !stage.passthrough_eligible() {
                return Ok(None);
            }
            stage.request_handoff();
            if !self.adopt_handed_off_decoder() {
                return Ok(None);
            }
        }
        if !self.eligible {
            return Ok(None);
        }
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
            // The stream's first packet starts before media sample 0, so the very start of
            // playback is a packet boundary even though it is off the grid.
            let draining_junction = self.junction;
            if draining_junction {
                if !pcm.is_empty() {
                    return Ok(None);
                }
            } else {
                let at_stream_start = decoder.at_first_packet() && processor.next_sample() == 0;
                let junction_path = self.pre_skip == OPUS_ENCODER_LOOKAHEAD && self.pcm_run > 0;
                // After a re-encoded stretch the output timeline runs a lookahead ahead of the
                // source, so the packet boundary is judged on the source's own position.
                let on_grid = if junction_path {
                    (self.consumed + u64::from(self.pre_skip)).is_multiple_of(960)
                } else {
                    (processor.next_sample() + u64::from(self.pre_skip)).is_multiple_of(960)
                };
                if !pcm.is_empty() || decoder.has_pending_frames() || !(on_grid || at_stream_start)
                {
                    return Ok(None);
                }
                if self.consumed > 0 && decoder.source_boundary_sample() != self.boundary {
                    return Ok(None);
                }
                if junction_path {
                    // The encoder lags its input by its lookahead and carries state the client
                    // decoder has settled on. A seamless return needs it started on the media
                    // grid, warm, and fed the next packet's first samples; anything else keeps
                    // decoding rather than leave a hole or a cold decoder at the seam.
                    let clear_of_overlap = before_overlap.is_none_or(|left| left > 2 * 960);
                    let phase_ready = self.encoder_phase.is_some_and(|phase| {
                        (phase + u64::from(self.pre_skip)).is_multiple_of(960)
                    });
                    if !phase_ready
                        || self.pcm_run < u64::from(OPUS_RECOVERY_WARM_FRAMES) * 960
                        || !clear_of_overlap
                    {
                        return Ok(None);
                    }
                    return match decoder.overshoot_next_packet(OPUS_ENCODER_LOOKAHEAD as usize)? {
                        Some(head) => {
                            pcm.extend(head.frame.samples);
                            self.junction = true;
                            Ok(None)
                        }
                        None => Ok(None),
                    };
                }
            }
            decoder.deactivate_decoder()?;
            if draining_junction && std::mem::take(&mut self.truthful) {
                // The drained samples were accounted once as encoder input and are about to be
                // sent again as part of the next packet.
                processor.rewind(u64::from(OPUS_ENCODER_LOOKAHEAD));
            }
            encoded_output.prepare_passthrough(processor.next_sample(), cancellation)?;
            if draining_junction {
                // The drained samples are sent again as part of the packet that follows.
                self.consumed = self
                    .consumed
                    .saturating_sub(u64::from(OPUS_ENCODER_LOOKAHEAD));
                self.boundary = Some(self.consumed);
                self.junction = false;
            }
            self.pcm_run = 0;
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
        let usable = encoded.as_ref().is_some_and(|encoded| {
            !packet.has_trimming_metadata && encoded_output.supports_encoded_packet(encoded)
        });
        let fits = usable
            && encoded.as_ref().is_some_and(|encoded| {
                before_overlap
                    .is_none_or(|left| u64::from(encoded.media_samples_per_channel) <= left)
            });
        if !fits || !still_wanted {
            decoder.queue_compressed_packet(packet);
            if !fits {
                self.eligible = false;
            }
            if !usable {
                // A packet the output can never forward disables passthrough for this stream,
                // unlike one that only would not fit before the overlap.
                self.stream_eligible = false;
            }
            return Ok(None);
        }
        let mut encoded = encoded.expect("checked packet eligibility");
        encoded.start_sample = processor
            .record_passthrough_packet(
                encoded.media_samples_per_channel,
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
                if self.junction && pcm.is_empty() {
                    // Packets were not resumed (for example a gain change arrived). The decoder
                    // already holds the next packet's state, so rebuild it from the history before
                    // that packet and skip the samples already sent, then carry on decoding.
                    let boundary = decoder.source_boundary_sample().unwrap_or(0);
                    let history = decoder.packet_history_before(boundary);
                    decoder.deactivate_decoder()?;
                    decoder.activate_decoder(
                        &history,
                        Some(boundary + u64::from(OPUS_ENCODER_LOOKAHEAD)),
                    )?;
                    self.junction = false;
                }
                if !self.junction {
                    while pcm.len() < wanted * 2 && !*ended && !cancellation.is_cancelled() {
                        match decoder.next_frame()? {
                            Some(frame) => pcm.extend(frame.samples),
                            None => *ended = true,
                        }
                    }
                }
                if pcm.is_empty() && !self.junction {
                    return Ok(None);
                }
                // While draining the encoder lookahead only the queued samples are returned.
                pcm.drain(..(wanted * 2).min(pcm.len())).collect()
            }
            ActiveInput::Prepared(stage) => {
                match stage.buffer.read_for_playback(wanted, cancellation) {
                    Some(frame) => frame,
                    None if cancellation.is_cancelled() => return Err(PlaybackError::Cancelled),
                    None if stage.handoff_complete() => {
                        // The staged PCM ended because the decoder moved to this source, not
                        // because the track did. Continue decoding it here.
                        self.adopt_handed_off_decoder();
                        return self.read(wanted, cancellation);
                    }
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
        self.pcm_run += (samples.len() / 2) as u64;
        self.encoder_phase = self
            .encoder_phase
            .map(|phase| (phase + (samples.len() / 2) as u64) % 960);
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
                // The demuxer is asked first, so a seek that fails leaves this source untouched,
                // forwarded packets included.
                decoder.seek(position)?;
                decoder.activate_decoder(&[], None)?;
                self.encoded = false;
                // The first packet after a demuxer seek needs trimming, so it is decoded. Once
                // that audio is consumed on a packet boundary the stream can forward packets
                // again if it ever could.
                self.eligible = self.stream_eligible && self.pre_skip == OPUS_ENCODER_LOOKAHEAD;
                self.boundary = None;
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
                self.stream_eligible = decoder.opus_passthrough_rejection().is_none();
                self.pre_skip = decoder.opus_pre_skip();
                self.eligible = self.stream_eligible && self.pre_skip == OPUS_ENCODER_LOOKAHEAD;
                self.boundary = None;
                stage.cancel_and_join();
                self.input = ActiveInput::Direct {
                    decoder: Box::new(decoder),
                    pcm: VecDeque::new(),
                    ended: false,
                };
            }
        };
        self.encoded = false;
        self.junction = false;
        self.truthful = false;
        self.prime = None;
        self.pcm_run = 0;
        self.encoder_phase = None;
        self.probe = None;
        self.effective_end = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_and_unknown_duration_sources_do_not_start_probes() {
        let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
        for source in [
            InputSource::http_url("https://invalid.example/expiring"),
            InputSource::sequential_http_url("https://invalid.example/single-use"),
        ] {
            let decoder = AudioDecoder::open_for_playback(
                InputSource::bytes(fixture.to_vec()),
                DecoderOptions::default(),
            )
            .unwrap();
            let mut active = ActiveSource::decoder(decoder, source);
            let duration = active.duration_samples;
            assert_eq!(active.scheduling_end(true, 48_000), duration);
            assert!(active.probe.is_none());
            active.duration_samples = None;
            assert_eq!(active.scheduling_end(true, 48_000), None);
            assert!(active.probe.is_none());
        }
    }

    #[test]
    fn no_stage_keeps_local_opus_eligible_without_analysis() {
        let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
        let source = InputSource::bytes(fixture.to_vec());
        let decoder =
            AudioDecoder::open_for_playback(source.clone(), DecoderOptions::default()).unwrap();
        let mut active = ActiveSource::decoder(decoder, source);
        assert!(active.passthrough_eligible());
        assert_eq!(
            active.scheduling_end(false, 48_000),
            active.duration_samples
        );
        assert!(active.probe.is_none());
        assert!(active.passthrough_eligible());
    }
}
