use super::*;

impl MediaOutput for OpusEncoderOutput {
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_nonblocking(&self) -> bool {
        self.sink.inner.scheduler_nonblocking()
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_activate(&self) {
        self.sink.activate();
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_ready(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        self.poll_production(cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_control(
        &self,
        control: crate::OutputControl,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        self.poll_barrier(control, cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn wait_ready(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if !self.sink.active() {
            return Ok(());
        }
        struct ThreadWake(std::thread::Thread);
        impl std::task::Wake for ThreadWake {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                self.0.unpark();
            }
        }
        impl crate::source::CancellationWake for ThreadWake {
            fn wake_for_cancellation(&self) {
                self.0.unpark();
            }
        }
        let wake = std::sync::Arc::new(ThreadWake(std::thread::current()));
        cancellation.register_waker(wake.clone());
        let waker = std::task::Waker::from(wake);
        let mut cx = std::task::Context::from_waker(&waker);
        loop {
            match self.poll_production(&mut cx, cancellation) {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => std::thread::park(),
            }
        }
    }

    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        if frame.samples_per_channel.checked_mul(2) != Some(frame.samples.len()) {
            return Err(OutputError::new(
                "PCM frame does not contain stereo samples",
            ));
        }
        #[cfg(feature = "experimental-scheduler")]
        if self.sink.active() {
            let mut pending = lock(&self.pending_pcm);
            if pending.is_some() {
                return Err(OutputError::new("PCM written before output became ready"));
            }
            *pending = Some((frame, 0));
            return Ok(());
        }
        let mut state = lock(&self.state);
        self.initialize_encoder(&mut state)?;
        Self::write_pcm(
            &mut state,
            &frame.samples,
            frame.start_sample,
            &self.sink,
            cancellation,
        )
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
        #[cfg(feature = "experimental-scheduler")]
        {
            lock(&self.pending_pcm).take();
        }
        self.sink.flush_samples(sample_position, cancellation)?;
        let mut state = lock(&self.state);
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.partial_scalars = 0;
        state.partial_start_sample = None;
        state.partial_lead_samples = 0;
        state.suppressed_frames = 0;
        state.primed_unflushed = false;
        state.start_bias = 0;
        state.partial_is_alignment_only = false;
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
        state.discard_alignment_padding();
        if state.primed_unflushed {
            // No packet has been sent since the encoder was primed, so the live audio is still in
            // its lookahead and the client has not heard it. Finish the frame that only carries
            // audio the client already has without sending it, then push the lookahead out with a
            // frame of silence that reports just the live samples as media.
            let live = state
                .primed_live_samples
                .min(crate::decoder::OPUS_ENCODER_LOOKAHEAD as usize);
            // The live samples start where the client's audio ends: one lookahead before the
            // encoder's input position minus whatever was already fed.
            let start_sample = state
                .next_input_sample
                .unwrap_or(0)
                .saturating_sub(state.primed_live_samples as u64);
            let partial_scalars = state.partial_scalars;
            let mut padded = state.partial;
            let silence = [0.0_f32; OPUS_FRAME_SCALARS];
            let encoder = state
                .encoder
                .as_mut()
                .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?;
            if partial_scalars != 0 {
                padded[partial_scalars..].fill(0.0);
                Self::encode_and_send(
                    encoder,
                    &padded,
                    OPUS_FRAME_SAMPLES,
                    0,
                    start_sample,
                    true,
                    &self.sink,
                    cancellation,
                )?;
            }
            if live != 0 {
                Self::encode_and_send(
                    encoder,
                    &silence,
                    OPUS_FRAME_SAMPLES,
                    live,
                    start_sample,
                    false,
                    &self.sink,
                    cancellation,
                )?;
            }
            state.suppressed_frames = 0;
            state.primed_unflushed = false;
            state.partial_scalars = 0;
            state.partial_lead_samples = 0;
            state.partial_start_sample = None;
        }
        if state.partial_scalars != 0 {
            let media_samples = state.partial_scalars / 2 - state.partial_lead_samples;
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
                false,
                &self.sink,
                cancellation,
            )?;
            state.partial_scalars = 0;
            state.partial_lead_samples = 0;
        }
        #[cfg(feature = "experimental-scheduler")]
        if self.sink.active() {
            state.finished = true;
            return Ok(());
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
