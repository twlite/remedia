use super::*;

impl EncodedOpusOutput for OpusEncoderOutput {
    fn supports_encoded_packet(&self, packet: &EncodedOpusPacket) -> bool {
        matches!(
            packet.samples_per_channel,
            120 | 240 | 480 | 960 | 1_920 | 2_880 | 5_760
        ) && (1..=packet.samples_per_channel).contains(&packet.media_samples_per_channel)
            && !packet.bytes.is_empty()
            && self.sink.supports_passthrough_packet(packet)
    }

    fn write_encoded_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        if !self.supports_encoded_packet(&packet) {
            return Err(OutputError::new(
                "Opus packet is not compatible with this output sink",
            ));
        }
        let mut state = lock(&self.state);
        if state.finished {
            return Err(OutputError::new(
                "Opus packet written after output finished",
            ));
        }
        if state.partial_scalars != 0 && !state.partial_is_alignment_only {
            return Err(OutputError::new(
                "cannot pass through Opus while a partial PCM frame is pending",
            ));
        }
        state.discard_alignment_padding();
        if let Some(expected) = state.next_input_sample
            && expected != packet.start_sample
        {
            tracing::error!(
                name: "core.opus.packet_discontinuity",
                target: "remedia_core::opus",
                expected_sample = expected,
                received_sample = packet.start_sample,
                "Opus output rejected a discontinuous encoded packet timeline"
            );
            return Err(OutputError::new(format!(
                "Opus timeline is discontinuous: expected sample {expected}, received {}",
                packet.start_sample
            )));
        }
        let end_sample = packet
            .start_sample
            .saturating_add(u64::from(packet.media_samples_per_channel));
        self.sink.send_packet(packet, cancellation)?;
        state.next_input_sample = Some(end_sample);
        Ok(())
    }

    fn prepare_passthrough(
        &self,
        next_sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        // After `begin_seamless_pcm` the caller counts heard media, which is one lookahead behind
        // the encoder's input position.
        if let Some(expected) = state.next_input_sample
            && expected != next_sample_position
            && expected.saturating_sub(state.start_bias) != next_sample_position
        {
            return Err(OutputError::new(format!(
                "passthrough transition expected sample {expected}, received {next_sample_position}"
            )));
        }
        state.discard_alignment_padding();
        if state.partial_scalars != 0 {
            let total_samples = state.partial_scalars / 2;
            let media_samples = total_samples - state.partial_lead_samples;
            let encoded_samples = [120, 240, 480, OPUS_FRAME_SAMPLES]
                .into_iter()
                .find(|samples| *samples >= total_samples)
                .ok_or_else(|| OutputError::new("partial Opus frame is too large to flush"))?;
            let encoded_scalars = encoded_samples * 2;
            let partial_scalars = state.partial_scalars;
            state.partial[partial_scalars..encoded_scalars].fill(0.0);
            let start_sample = state.partial_start_sample.unwrap_or(next_sample_position);
            let pcm = state.partial;
            let encoder = state
                .encoder
                .as_mut()
                .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?;
            Self::encode_and_send(
                encoder,
                &pcm[..encoded_scalars],
                encoded_samples,
                media_samples,
                start_sample,
                false,
                &self.sink,
                cancellation,
            )?;
            state.partial_scalars = 0;
            state.partial_start_sample = None;
            state.partial_lead_samples = 0;
        }
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.next_input_sample = Some(next_sample_position);
        state.start_bias = 0;
        state.primed_unflushed = false;
        state.suppressed_frames = 0;
        Ok(())
    }

    fn align_encoder_frames(&self, next_sample_position: u64) -> Result<(), OutputError> {
        let lead = (next_sample_position % OPUS_FRAME_SAMPLES as u64) as usize;
        let mut state = lock(&self.state);
        if lead == 0 || state.partial_scalars != 0 {
            return Ok(());
        }
        state.partial[..lead * 2].fill(0.0);
        state.partial_scalars = lead * 2;
        state.partial_lead_samples = lead;
        state.partial_start_sample = Some(next_sample_position);
        state.partial_is_alignment_only = true;
        Ok(())
    }

    fn begin_seamless_pcm(
        &self,
        history_pcm: &[f32],
        live_start_sample: u64,
        cancellation: &CancellationToken,
    ) -> Result<bool, OutputError> {
        self.ensure_active(cancellation)?;
        let history = history_pcm.len() / 2;
        let lookahead = crate::decoder::OPUS_ENCODER_LOOKAHEAD as usize;
        // The history must leave the encoder's partial frame exactly one lookahead short of full
        // and be long enough for the encoder to settle before the first packet is sent.
        if !history_pcm.len().is_multiple_of(2)
            || history % OPUS_FRAME_SAMPLES != OPUS_FRAME_SAMPLES - lookahead
            || history < 5 * OPUS_FRAME_SAMPLES
            || live_start_sample < history as u64
        {
            return Ok(false);
        }
        let mut state = lock(&self.state);
        if state.finished {
            return Ok(false);
        }
        self.initialize_encoder(&mut state)?;
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.partial_scalars = 0;
        state.partial_lead_samples = 0;
        state.partial_start_sample = None;
        state.partial_is_alignment_only = false;
        // Every frame that finishes before the lookahead is drained carries audio already played.
        state.suppressed_frames = ((history + lookahead) / OPUS_FRAME_SAMPLES) as u32;
        state.next_input_sample = Some(live_start_sample - history as u64);
        Self::write_pcm(
            &mut state,
            history_pcm,
            live_start_sample - history as u64,
            &self.sink,
            cancellation,
        )?;
        state.primed_unflushed = true;
        state.primed_live_samples = 0;
        state.start_bias = lookahead as u64;
        Ok(true)
    }
}
