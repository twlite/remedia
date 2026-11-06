use super::*;

impl EncodedOpusOutput for OpusEncoderOutput {
    fn supports_encoded_packet(&self, packet: &EncodedOpusPacket) -> bool {
        matches!(
            packet.samples_per_channel,
            120 | 240 | 480 | 960 | 1_920 | 2_880 | 5_760
        ) && packet.media_samples_per_channel == packet.samples_per_channel
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
        if state.partial_scalars != 0 {
            return Err(OutputError::new(
                "cannot pass through Opus while a partial PCM frame is pending",
            ));
        }
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
        if let Some(expected) = state.next_input_sample
            && expected != next_sample_position
        {
            return Err(OutputError::new(format!(
                "passthrough transition expected sample {expected}, received {next_sample_position}"
            )));
        }
        if state.partial_scalars != 0 {
            let media_samples = state.partial_scalars / 2;
            let encoded_samples = [120, 240, 480, OPUS_FRAME_SAMPLES]
                .into_iter()
                .find(|samples| *samples >= media_samples)
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
                &*self.sink,
                cancellation,
            )?;
            state.partial_scalars = 0;
            state.partial_start_sample = None;
        }
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.next_input_sample = Some(next_sample_position);
        Ok(())
    }
}
