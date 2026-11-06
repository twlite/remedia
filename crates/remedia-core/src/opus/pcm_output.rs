use super::*;

impl MediaOutput for OpusEncoderOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        self.initialize_encoder(&mut state)?;
        Self::write_pcm(&mut state, frame, &*self.sink, cancellation)
    }

    fn encoded_opus_output(&self) -> Option<&dyn EncodedOpusOutput> {
        Some(self)
    }

    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.flush_samples(duration_to_samples(position)?, cancellation)
    }

    fn flush_samples(
        &self,
        sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.flush_samples(sample_position, cancellation)?;
        let mut state = lock(&self.state);
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.partial_scalars = 0;
        state.partial_start_sample = None;
        state.next_input_sample = Some(sample_position);
        state.finished = false;
        Ok(())
    }

    fn filter_transition(
        &self,
        next_sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        if let Some(expected) = state.next_input_sample
            && expected != next_sample_position
        {
            tracing::error!(
                name: "core.opus.filter_transition_discontinuity",
                target: "remedia_core::opus",
                expected_sample = expected,
                received_sample = next_sample_position,
                "filter transition did not match the next Opus PCM sample"
            );
            return Err(OutputError::new(format!(
                "filter transition expected sample {expected}, received {next_sample_position}"
            )));
        }
        // A partial Opus frame is already accepted PCM. Keep it so it can be
        // completed by the next graph's contiguous samples. This avoids a seek
        // flush, encoder reset, and Discord RTP generation change.
        state.next_input_sample = Some(next_sample_position);
        Ok(())
    }

    fn pause(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.pause(cancellation)
    }

    fn resume(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.resume(cancellation)
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        if state.finished {
            return Err(OutputError::new("Opus output finished more than once"));
        }
        if state.partial_scalars != 0 {
            let media_samples = state.partial_scalars / 2;
            let partial_scalars = state.partial_scalars;
            state.partial[partial_scalars..].fill(0.0);
            let start_sample = state.partial_start_sample.take().unwrap_or(0);
            with_dispatch(&self.observability, || {
                tracing::debug!(
                    name: "core.opus.final_frame_padded",
                    target: "remedia_core::opus",
                    media_samples_per_channel = media_samples,
                    encoded_samples_per_channel = OPUS_FRAME_SAMPLES,
                    start_sample,
                    "final partial PCM frame padded for Opus encoding"
                );
            });
            let padded = state.partial;
            let encoder = state
                .encoder
                .as_mut()
                .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?;
            Self::encode_and_send(
                encoder,
                &padded,
                OPUS_FRAME_SAMPLES,
                media_samples,
                start_sample,
                &*self.sink,
                cancellation,
            )?;
            state.partial_scalars = 0;
        }
        self.sink.finish(cancellation)?;
        state.finished = true;
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.sink.cancel();
        with_dispatch(&self.observability, || {
            tracing::debug!(
                name: "core.opus.output_cancelled",
                target: "remedia_core::opus",
                "Opus output cancelled"
            );
        });
    }

    fn played_position(&self) -> Option<Duration> {
        self.sink.played_position()
    }
}
