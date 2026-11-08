use super::*;

impl PlaybackSession {
    /// Request a pause and wait until the worker reaches a frame boundary.
    ///
    /// The call can wait for an in-progress decoder read or output write to
    /// return. The worker then waits on a condition variable and does not decode
    /// or queue more audio until resumed.
    pub fn pause(&self) -> Result<(), PlaybackError> {
        let mut data = lock(&self.shared.data);
        match data.state {
            PlaybackState::Paused => return Ok(()),
            PlaybackState::Playing => {
                data.state = PlaybackState::Pausing;
                self.shared.changed.notify_all();
            }
            PlaybackState::Pausing => {}
            state => {
                return Err(PlaybackError::InvalidState {
                    operation: "pause",
                    state,
                });
            }
        }

        while data.state == PlaybackState::Pausing {
            data = wait(&self.shared.changed, data);
        }
        match data.state {
            PlaybackState::Paused => Ok(()),
            PlaybackState::Failed => Err(data
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
            PlaybackState::Stopping => Err(PlaybackError::Cancelled),
            PlaybackState::Stopped => Err(PlaybackError::Cancelled),
            state => Err(PlaybackError::InvalidState {
                operation: "pause",
                state,
            }),
        }
    }

    /// Resume a paused session. Calling this while already playing is harmless.
    pub fn resume(&self) -> Result<(), PlaybackError> {
        let mut data = lock(&self.shared.data);
        match data.state {
            PlaybackState::Playing => return Ok(()),
            PlaybackState::Paused => {
                data.state = PlaybackState::Resuming;
                self.shared.changed.notify_all();
            }
            PlaybackState::Resuming => {}
            state => {
                return Err(PlaybackError::InvalidState {
                    operation: "resume",
                    state,
                });
            }
        }

        while data.state == PlaybackState::Resuming {
            data = wait(&self.shared.changed, data);
        }
        match data.state {
            PlaybackState::Playing => Ok(()),
            PlaybackState::Failed => Err(data
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
            PlaybackState::Stopping | PlaybackState::Stopped => Err(PlaybackError::Cancelled),
            state => Err(PlaybackError::InvalidState {
                operation: "resume",
                state,
            }),
        }
    }

    /// Seek the active source. Seeking is supported while playing or paused.
    /// The call waits for the decoder and output to discard pre-seek data.
    pub fn seek(&self, position: Duration) -> Result<(), PlaybackError> {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let playback_id = {
            let mut data = lock(&self.shared.data);
            if !matches!(data.state, PlaybackState::Playing | PlaybackState::Paused) {
                return Err(PlaybackError::InvalidState {
                    operation: "seek",
                    state: data.state,
                });
            }
            if data.pending_seek.is_some() {
                return Err(PlaybackError::SeekPending);
            }
            if let Some(duration) = data.duration
                && position > duration
            {
                return Err(PlaybackError::SeekOutOfRange {
                    target: position,
                    duration,
                });
            }
            let position = normalize_position(position)?;
            data.pending_seek = Some(SeekRequest {
                position,
                response: response_tx,
            });
            self.shared.changed.notify_all();
            data.playback_id
        };
        with_dispatch(&self.shared.observability, || {
            tracing::info!(
                name: "core.playback.seek_requested",
                target: "remedia_core::playback",
                playback_id,
                position_ms = position.as_millis() as u64,
                "playback seek requested"
            );
        });

        let outcome = response_rx.recv().unwrap_or_else(|_| {
            Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked))
        });
        if outcome.is_ok() { self.clear_staged_source(); }
        outcome
    }

    /// Validate and configure a trusted FFmpeg audio filtergraph.
    ///
    /// The expression is parsed by native libavfilter and must describe one
    /// audio input and one audio output. `None` clears filtering. Empty or
    /// whitespace-only strings are rejected. When active, replacement waits
    /// for a PCM boundary and drops delayed output still held inside the
    /// previous graph. PCM already accepted by the output is retained. The
    /// source is not reopened. Graph strings are trusted application
    /// configuration; some FFmpeg filters can access files or URLs.
    pub fn set_filter_graph(&self, expression: Option<&str>) -> Result<(), PlaybackError> {
        let _update = lock(&self.filter_update_lock);
        let expression = expression.map(str::to_owned);
        let candidate = with_dispatch(&self.shared.observability, || {
            expression.as_deref().map(PcmFilterGraph::build).transpose()
        });
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                let playback_id = lock(&self.shared.data).playback_id;
                with_dispatch(&self.shared.observability, || {
                    tracing::warn!(
                        name: "core.playback.filter_graph_replacement_failed",
                        target: "remedia_core::playback",
                        playback_id,
                        error_kind = "invalid_graph",
                        "replacement filtergraph could not be built; active graph was retained"
                    );
                });
                return Err(PlaybackError::FilterGraph(Arc::new(error)));
            }
        };
        let mut data = lock(&self.shared.data);
        if data.filter_graph == expression {
            return Ok(());
        }
        data.last_processing_error = None;
        let playback_id = data.playback_id;
        let graph_enabled = expression.is_some();
        data.filter_graph = expression;
        let active = is_active(data.state) && !data.stopping;
        let response = if active && data.processor_initialized {
            let (sender, receiver) = mpsc::sync_channel(1);
            data.pending_filter_graph = Some(FilterUpdate {
                graph: candidate,
                response: sender,
            });
            self.shared.changed.notify_all();
            Some(receiver)
        } else {
            None
        };
        drop(data);
        with_dispatch(&self.shared.observability, || {
            tracing::info!(
                name: "core.playback.filter_graph_replacement_requested",
                target: "remedia_core::playback",
                playback_id,
                graph_enabled,
                active,
                "filtergraph replacement requested"
            );
        });
        if let Some(receiver) = response {
            return receiver.recv().unwrap_or_else(|_| {
                let data = lock(&self.shared.data);
                if data.stopping
                    || matches!(data.state, PlaybackState::Stopping | PlaybackState::Stopped)
                {
                    Err(PlaybackError::Cancelled)
                } else {
                    Err(data
                        .last_error
                        .clone()
                        .unwrap_or(PlaybackError::WorkerPanicked))
                }
            });
        }
        Ok(())
    }

    /// Return the configured graph expression, if any.
    pub fn filter_graph(&self) -> Option<String> {
        lock(&self.shared.data).filter_graph.clone()
    }

    /// Set master gain independently of the raw FFmpeg filtergraph.
    ///
    /// The accepted linear amplitude range is `0.0..=2.0`. Samples are hard
    /// clipped to `[-1.0, 1.0]`; this is not a limiter or loudness normalizer.
    /// Changes take effect at the next processed PCM frame and persist across
    /// tracks in this session. If the change requires entering or leaving Opus
    /// passthrough, this call waits for that worker transition and returns its
    /// decoder/output error. The wait can include a blocking input read or
    /// bounded-output backpressure; `stop` cancels it. While paused, the value
    /// is staged for the resume boundary instead of making this call wait.
    pub fn set_volume(&self, gain: f32) -> Result<(), PlaybackError> {
        if !gain.is_finite() || !(0.0..=2.0).contains(&gain) {
            return Err(PlaybackError::InvalidVolume(gain));
        }
        let (generation, target_mode) = {
            let mut data = lock(&self.shared.data);
            if data.volume == gain {
                return Ok(());
            }
            data.last_processing_error = None;
            data.volume = gain;
            let target_mode = data.passthrough_available.then_some(
                if data.filter_graph.is_none() && gain == 1.0 {
                    PlaybackProcessingMode::Passthrough
                } else {
                    PlaybackProcessingMode::Transcoding
                },
            );
            let target_mode = target_mode.filter(|target| {
                data.processor_initialized
                    && data.state == PlaybackState::Playing
                    && data.processing_mode != Some(*target)
            });
            self.shared.changed.notify_all();
            (data.generation, target_mode)
        };

        if let Some(target_mode) = target_mode {
            let mut data = lock(&self.shared.data);
            loop {
                if data.generation != generation {
                    return Err(PlaybackError::Cancelled);
                }
                if let Some(error) = data.last_processing_error.take() {
                    return Err(error);
                }
                if data.processing_mode == Some(target_mode) {
                    return Ok(());
                }
                if data.stopping
                    || matches!(data.state, PlaybackState::Stopping | PlaybackState::Stopped)
                {
                    return Err(PlaybackError::Cancelled);
                }
                if data.state == PlaybackState::Failed {
                    return Err(data
                        .last_error
                        .clone()
                        .unwrap_or(PlaybackError::WorkerPanicked));
                }
                if !is_active(data.state) {
                    // Natural EOF may win the race. The configured gain is
                    // retained for the next source on this session.
                    return Ok(());
                }
                data = wait(&self.shared.changed, data);
            }
        }
        Ok(())
    }

    /// Current normalized linear master gain.
    pub fn volume(&self) -> f32 {
        lock(&self.shared.data).volume
    }

    /// Validate a graph without creating a playback session.
    pub fn validate_filter_graph(expression: &str) -> Result<(), FilterGraphError> {
        PcmFilterGraph::build(expression).map(|_| ())
    }

    /// Stop active work, cancel pending I/O/output, and join its worker.
    ///
    /// Stopping an idle session changes it to `Stopped`. Repeated stops are
    /// harmless. `Completed` and `Failed` remain terminal so their outcome and
    /// error remain observable.
    pub fn stop(&self) -> Result<(), PlaybackError> {
        let (
            needs_cleanup,
            active,
            playback_id,
            cancellation,
            output,
            pending_seek,
            pending_filter,
        ) = {
            let mut data = lock(&self.shared.data);
            while data.stopping {
                data = wait(&self.shared.changed, data);
            }

            let active = is_active(data.state);
            let needs_cleanup =
                active || matches!(data.state, PlaybackState::Completed | PlaybackState::Failed);
            if data.state == PlaybackState::Idle {
                data.state = PlaybackState::Stopped;
            }
            if needs_cleanup {
                data.stopping = true;
            }
            if active {
                data.state = PlaybackState::Stopping;
            }
            let cancellation = if active {
                data.cancellation.clone()
            } else {
                None
            };
            let output = data.output.take();
            let pending_seek = data.pending_seek.take();
            let pending_filter = data.pending_filter_graph.take();
            let playback_id = data.playback_id;
            self.shared.changed.notify_all();
            (
                needs_cleanup,
                active,
                playback_id,
                cancellation,
                output,
                pending_seek,
                pending_filter,
            )
        };

        // After the state is marked Stopping, no concurrent stage request can
        // become eligible. Clear the stage outside the playback-state lock.
        self.clear_staged_source();

        if let Some(request) = pending_seek {
            let _ = request.response.send(Err(PlaybackError::Cancelled));
        }
        if let Some(update) = pending_filter {
            let _ = update.response.send(Err(PlaybackError::Cancelled));
        }
        if let Some(cancellation) = &cancellation {
            with_dispatch(&self.shared.observability, || {
                tracing::debug!(
                    name: "core.playback.cancellation_requested",
                    target: "remedia_core::playback",
                    playback_id,
                    "playback cancellation requested"
                );
            });
            cancellation.cancel();
        }
        let output_cancel_panicked = cancellation.is_some()
            && output
                .as_ref()
                .is_some_and(|output| safe_cancel(output).is_err());

        let worker = lock(&self.worker).take();
        let worker_panicked = worker.is_some_and(|worker| worker.join().is_err());
        let played_position = output.as_ref().and_then(safe_played_position);

        let mut data = lock(&self.shared.data);
        if let Some(position) = played_position {
            data.played_position = Some(position);
        }
        data.cancellation = None;
        data.output = None;
        if needs_cleanup {
            data.stopping = false;
        }
        if output_cancel_panicked || worker_panicked {
            let error = PlaybackError::WorkerPanicked;
            data.state = PlaybackState::Failed;
            data.last_error = Some(error.clone());
            self.shared.changed.notify_all();
            return Err(error);
        }
        if active {
            data.state = PlaybackState::Stopped;
        }
        let terminal_state = data.state;
        self.shared.changed.notify_all();
        drop(data);
        if needs_cleanup {
            with_dispatch(&self.shared.observability, || {
                tracing::info!(
                    name: "core.playback.stopped",
                    target: "remedia_core::playback",
                    playback_id,
                    state = %terminal_state,
                    "playback worker stopped and resources released"
                );
            });
        }
        Ok(())
    }
}
