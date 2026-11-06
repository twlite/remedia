use super::*;

impl Default for PlaybackSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackSession {
    /// Create an idle playback session. Sessions do not share playback state.
    pub fn new() -> Self {
        Self::with_observability(ObservabilityHandle::default())
    }

    /// Create an idle session attached to an application-owned observability context.
    ///
    /// The handle controls which local tracing subscriber and metric provider receive events. It
    /// does not install a process-global subscriber.
    pub fn with_observability(observability: ObservabilityHandle) -> Self {
        Self {
            shared: Arc::new(SessionShared {
                data: Mutex::new(SessionData {
                    state: PlaybackState::Idle,
                    generation: 0,
                    playback_id: 0,
                    processing_mode: None,
                    passthrough_available: false,
                    last_processing_error: None,
                    duration: None,
                    info: None,
                    submitted_position: Duration::ZERO,
                    submitted_sample: 0,
                    media_position: None,
                    played_position: None,
                    last_error: None,
                    cancellation: None,
                    output: None,
                    pending_seek: None,
                    filter_graph: None,
                    pending_filter_graph: None,
                    processor_initialized: false,
                    volume: 1.0,
                    stopping: false,
                }),
                changed: Condvar::new(),
                observability,
            }),
            worker: Mutex::new(None),
            start_lock: Mutex::new(()),
            filter_update_lock: Mutex::new(()),
        }
    }

    /// Open a source and start it with an output owned by this playback.
    ///
    /// This call blocks until the FFmpeg input and audio decoder are initialized.
    /// It returns the selected stream metadata only after initialization succeeds.
    /// Calls from another thread may stop a source while it is opening.
    pub fn play<O: MediaOutput>(
        &self,
        source: InputSource,
        output: O,
    ) -> Result<AudioStreamInfo, PlaybackError> {
        self.play_with_cancellation(source, output, CancellationToken::new())
    }

    /// Open a source using a caller-owned cancellation signal.
    ///
    /// If the token is cancelled before the worker starts, playback does not
    /// begin. Once started, cancelling the token interrupts decoder I/O; call
    /// [`stop`](Self::stop) to join the worker and release its output.
    pub fn play_with_cancellation<O: MediaOutput>(
        &self,
        source: InputSource,
        output: O,
        cancellation: CancellationToken,
    ) -> Result<AudioStreamInfo, PlaybackError> {
        let start_guard = lock(&self.start_lock);
        if cancellation.is_cancelled() {
            return Err(PlaybackError::Cancelled);
        }
        {
            let data = lock(&self.shared.data);
            if cancellation.is_cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if data.stopping || is_active(data.state) {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }
        }
        self.reap_previous_worker()?;
        if cancellation.is_cancelled() {
            return Err(PlaybackError::Cancelled);
        }

        let output: Arc<dyn MediaOutput> = Arc::new(output);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let source_kind = source.kind();
        let (generation, playback_id) = {
            let mut data = lock(&self.shared.data);
            if cancellation.is_cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if data.stopping {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }
            if is_active(data.state) {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }

            data.generation = data.generation.wrapping_add(1);
            let generation = data.generation;
            let playback_id = next_correlation_id();
            data.playback_id = playback_id;
            data.processing_mode = None;
            data.passthrough_available = false;
            data.last_processing_error = None;
            data.state = PlaybackState::Starting;
            data.duration = None;
            data.info = None;
            data.submitted_position = Duration::ZERO;
            data.submitted_sample = 0;
            data.media_position = None;
            data.played_position = None;
            data.last_error = None;
            data.cancellation = Some(cancellation.clone());
            data.output = Some(Arc::clone(&output));
            data.pending_seek = None;
            data.processor_initialized = false;

            let shared = Arc::clone(&self.shared);
            let worker_output = Arc::clone(&output);
            let worker_cancellation = cancellation.clone();
            let worker_observability = self.shared.observability.clone();
            let thread_result = thread::Builder::new()
                .name(format!("remedia-playback-{generation}"))
                .spawn(move || {
                    with_dispatch(&worker_observability, || {
                        let span = tracing::info_span!(
                            target: "remedia_core::playback",
                            "core.playback.session",
                            playback_id,
                            generation,
                            otel.status_code = tracing::field::Empty,
                            error.type = tracing::field::Empty,
                        );
                        let _entered = span.enter();
                        let span_shared = Arc::clone(&shared);
                        worker_entry(
                            shared,
                            generation,
                            source,
                            worker_output,
                            worker_cancellation,
                            ready_tx,
                        );
                        let data = lock(&span_shared.data);
                        if data.generation == generation {
                            match data.state {
                                PlaybackState::Completed => {
                                    span.record("otel.status_code", "OK");
                                }
                                PlaybackState::Failed => {
                                    span.record("otel.status_code", "ERROR");
                                    if let Some(error) = &data.last_error {
                                        span.record("error.type", playback_error_kind(error));
                                    }
                                }
                                _ => {}
                            }
                        }
                    });
                });

            match thread_result {
                Ok(worker) => {
                    *lock(&self.worker) = Some(worker);
                    self.shared.changed.notify_all();
                }
                Err(error) => {
                    let playback_error = PlaybackError::WorkerInitialization(Arc::new(error));
                    data.state = PlaybackState::Failed;
                    data.last_error = Some(playback_error.clone());
                    data.cancellation = None;
                    data.output = None;
                    self.shared.changed.notify_all();
                    drop(data);
                    drop(start_guard);
                    self.shared.observability.metrics().playback_failed();
                    with_dispatch(&self.shared.observability, || {
                        tracing::error!(
                            name: "core.playback.worker_initialization_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            error_kind = "worker_initialization",
                            "playback worker could not be started"
                        );
                    });
                    let _ = safe_cancel(&output);
                    return Err(playback_error);
                }
            };
            (generation, playback_id)
        };

        drop(start_guard);
        with_dispatch(&self.shared.observability, || {
            tracing::info!(
                name: "core.playback.starting",
                target: "remedia_core::playback",
                playback_id,
                source_kind,
                "playback source opening"
            );
        });

        match ready_rx.recv() {
            Ok(Ok(info)) => {
                let mut data = lock(&self.shared.data);
                if data.generation != generation || data.state != PlaybackState::Starting {
                    return Err(data.last_error.clone().unwrap_or(PlaybackError::Cancelled));
                }
                data.duration = info.duration;
                data.info = Some(info.clone());
                data.state = PlaybackState::Playing;
                self.shared.changed.notify_all();
                drop(data);
                with_dispatch(&self.shared.observability, || {
                    tracing::info!(
                        name: "core.playback.started",
                        target: "remedia_core::playback",
                        playback_id,
                        codec = %info.codec,
                        duration_known = info.duration.is_some(),
                        "playback started"
                    );
                });
                Ok(info)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
        }
    }

    /// Return the current typed lifecycle state.
    pub fn state(&self) -> PlaybackState {
        lock(&self.shared.data).state
    }

    /// Return the current source processing mode when a playback source has
    /// started. `None` is returned before startup selects a path.
    pub fn processing_mode(&self) -> Option<PlaybackProcessingMode> {
        lock(&self.shared.data).processing_mode
    }

    /// Return submitted position and an optional output-reported played
    /// position. A buffering output may report `None` until it can determine
    /// which samples have actually played.
    pub fn position(&self) -> PlaybackPosition {
        let (media, submitted, cached_played, output) = {
            let data = lock(&self.shared.data);
            (
                data.media_position,
                data.submitted_position,
                data.played_position,
                data.output.clone(),
            )
        };
        let played = output
            .as_ref()
            .and_then(safe_played_position)
            .or(cached_played);
        PlaybackPosition {
            media,
            submitted,
            played,
        }
    }

    /// Return the selected media stream's duration when it is known.
    pub fn duration(&self) -> Option<Duration> {
        lock(&self.shared.data).duration
    }

    /// Return the selected stream metadata after startup succeeds.
    pub fn info(&self) -> Option<AudioStreamInfo> {
        lock(&self.shared.data).info.clone()
    }

    /// Return the most recent background failure, if the session failed.
    pub fn last_error(&self) -> Option<PlaybackError> {
        lock(&self.shared.data).last_error.clone()
    }

    /// Block until the session reaches a terminal state.
    pub fn wait(&self) -> PlaybackState {
        self.completion().wait()
    }

    /// Capture the active playback so a wait scheduled on another thread stays
    /// bound to that source even if it is stopped and replaced before waiting
    /// begins.
    pub fn completion(&self) -> PlaybackWaiter {
        self.completion_for_generation(self.completion_generation())
    }

    /// Return the current source generation for adapters that must capture a wait
    /// before scheduling blocking work on another runtime.
    pub fn completion_generation(&self) -> u64 {
        lock(&self.shared.data).generation
    }

    /// Bind a completion waiter to a generation captured by an adapter.
    pub fn completion_for_generation(&self, generation: u64) -> PlaybackWaiter {
        PlaybackWaiter {
            shared: Arc::clone(&self.shared),
            generation,
        }
    }

    pub(super) fn reap_previous_worker(&self) -> Result<(), PlaybackError> {
        let previous = lock(&self.worker).take();
        if previous.is_some_and(|worker| worker.join().is_err()) {
            return Err(PlaybackError::WorkerPanicked);
        }
        Ok(())
    }
}

impl PlaybackWaiter {
    /// Block until the captured source reaches a terminal state. If it has
    /// already been replaced, this returns `Stopped`.
    pub fn wait(&self) -> PlaybackState {
        let mut data = lock(&self.shared.data);
        while data.generation == self.generation && is_active(data.state) {
            data = wait(&self.shared.changed, data);
        }
        if data.generation != self.generation {
            return PlaybackState::Stopped;
        }
        data.state
    }
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
