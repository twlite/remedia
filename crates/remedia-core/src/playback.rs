use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;

use crate::decoder::{AudioDecoder, AudioStreamInfo, DecoderOptions, PcmFrame};
use crate::error::DecoderError;
use crate::output::{MediaOutput, OutputError};
use crate::source::{CancellationToken, InputSource};

/// Lifecycle state for one playback session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    /// No source has been started in this session.
    Idle,
    /// A worker is opening the source; playback has not started yet.
    Starting,
    /// The worker is decoding and delivering audio.
    Playing,
    /// A pause has been requested and is taking effect at a frame boundary.
    Pausing,
    /// The output is being resumed; frames are not delivered yet.
    Resuming,
    /// Decoding is suspended until [`PlaybackSession::resume`] is called.
    Paused,
    /// Active work is being cancelled and its worker is being joined.
    Stopping,
    /// Active work was stopped and its worker has been joined.
    Stopped,
    /// The decoder reached end-of-stream and the output finished successfully.
    Completed,
    /// An unrecoverable decoder, output, or worker failure occurred.
    Failed,
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Playing => "playing",
            Self::Pausing => "pausing",
            Self::Resuming => "resuming",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::Failed => "failed",
        })
    }
}

/// Playback errors retain decoder or output causes where available.
#[derive(Clone, Debug, Error)]
pub enum PlaybackError {
    #[error("decoder error: {0}")]
    Decoder(#[source] Arc<DecoderError>),
    #[error(transparent)]
    Output(#[from] OutputError),
    #[error("cannot {operation} while the session is {state}")]
    InvalidState {
        operation: &'static str,
        state: PlaybackState,
    },
    #[error("playback worker could not be started: {0}")]
    WorkerInitialization(#[source] Arc<std::io::Error>),
    #[error("playback worker panicked")]
    WorkerPanicked,
    #[error("playback operation was cancelled by stop")]
    Cancelled,
    #[error("another seek is already waiting to be applied")]
    SeekPending,
    #[error("seeking is not supported by this source: {0}")]
    UnsupportedSeeking(String),
    #[error("seek position {target:?} is beyond media duration {duration:?}")]
    SeekOutOfRange {
        target: Duration,
        duration: Duration,
    },
}

impl From<DecoderError> for PlaybackError {
    fn from(error: DecoderError) -> Self {
        match error {
            DecoderError::UnsupportedSeeking(message) => Self::UnsupportedSeeking(message),
            DecoderError::SeekOutOfRange { target, duration } => {
                Self::SeekOutOfRange { target, duration }
            }
            error => Self::Decoder(Arc::new(error)),
        }
    }
}

/// Positions reported by a session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaybackPosition {
    /// The end of the latest frame accepted by the output.
    pub submitted: Duration,
    /// The output's consumed/audible position, when it can report one.
    pub played: Option<Duration>,
}

/// A single-source playback session.
///
/// Each active session owns a blocking worker thread, decoder, and one output
/// instance. Frames are delivered synchronously, so backpressure stops decoding
/// without a session-level audio queue. A new `play` call is rejected while a
/// source is active; call `stop` before starting a replacement.
pub struct PlaybackSession {
    shared: Arc<SessionShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    start_lock: Mutex<()>,
}

struct SessionShared {
    data: Mutex<SessionData>,
    changed: Condvar,
}

struct SessionData {
    state: PlaybackState,
    generation: u64,
    duration: Option<Duration>,
    info: Option<AudioStreamInfo>,
    submitted_position: Duration,
    played_position: Option<Duration>,
    last_error: Option<PlaybackError>,
    cancellation: Option<CancellationToken>,
    output: Option<Arc<dyn MediaOutput>>,
    pending_seek: Option<SeekRequest>,
    stopping: bool,
}

struct SeekRequest {
    position: Duration,
    response: SyncSender<Result<(), PlaybackError>>,
}

enum WorkerAction {
    Decode,
    Deliver(PcmFrame),
    Seek(SeekRequest),
    Pause,
    Resume,
    Finish,
    Exit,
}

impl Default for PlaybackSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackSession {
    /// Create an idle playback session. Sessions do not share playback state.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(SessionShared {
                data: Mutex::new(SessionData {
                    state: PlaybackState::Idle,
                    generation: 0,
                    duration: None,
                    info: None,
                    submitted_position: Duration::ZERO,
                    played_position: None,
                    last_error: None,
                    cancellation: None,
                    output: None,
                    pending_seek: None,
                    stopping: false,
                }),
                changed: Condvar::new(),
            }),
            worker: Mutex::new(None),
            start_lock: Mutex::new(()),
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
        let start_guard = lock(&self.start_lock);
        {
            let data = lock(&self.shared.data);
            if data.stopping || is_active(data.state) {
                return Err(PlaybackError::InvalidState {
                    operation: "start playback",
                    state: data.state,
                });
            }
        }
        self.reap_previous_worker()?;

        let output: Arc<dyn MediaOutput> = Arc::new(output);
        let cancellation = CancellationToken::new();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let generation = {
            let mut data = lock(&self.shared.data);
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
            data.state = PlaybackState::Starting;
            data.duration = None;
            data.info = None;
            data.submitted_position = Duration::ZERO;
            data.played_position = None;
            data.last_error = None;
            data.cancellation = Some(cancellation.clone());
            data.output = Some(Arc::clone(&output));
            data.pending_seek = None;

            let shared = Arc::clone(&self.shared);
            let worker_output = Arc::clone(&output);
            let worker_cancellation = cancellation.clone();
            let thread_result = thread::Builder::new()
                .name(format!("remedia-playback-{generation}"))
                .spawn(move || {
                    worker_entry(
                        shared,
                        generation,
                        source,
                        worker_output,
                        worker_cancellation,
                        ready_tx,
                    );
                });

            match thread_result {
                Ok(worker) => {
                    *lock(&self.worker) = Some(worker);
                    self.shared.changed.notify_all();
                    generation
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
                    let _ = safe_cancel(&output);
                    return Err(playback_error);
                }
            }
        };

        drop(start_guard);

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
                Ok(info)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked)),
        }
    }

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
        {
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
        }

        response_rx.recv().unwrap_or_else(|_| {
            Err(lock(&self.shared.data)
                .last_error
                .clone()
                .unwrap_or(PlaybackError::WorkerPanicked))
        })
    }

    /// Stop active work, cancel pending I/O/output, and join its worker.
    ///
    /// Stopping an idle session changes it to `Stopped`. Repeated stops are
    /// harmless. `Completed` and `Failed` remain terminal so their outcome and
    /// error remain observable.
    pub fn stop(&self) -> Result<(), PlaybackError> {
        let (needs_cleanup, active, cancellation, output, pending_seek) = {
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
            self.shared.changed.notify_all();
            (needs_cleanup, active, cancellation, output, pending_seek)
        };

        if let Some(request) = pending_seek {
            let _ = request.response.send(Err(PlaybackError::Cancelled));
        }
        if let Some(cancellation) = &cancellation {
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
        self.shared.changed.notify_all();
        Ok(())
    }

    /// Return the current typed lifecycle state.
    pub fn state(&self) -> PlaybackState {
        lock(&self.shared.data).state
    }

    /// Return submitted position and an optional output-reported played
    /// position. A buffering output may report `None` until it can determine
    /// which samples have actually played.
    pub fn position(&self) -> PlaybackPosition {
        let (submitted, cached_played, output) = {
            let data = lock(&self.shared.data);
            (
                data.submitted_position,
                data.played_position,
                data.output.clone(),
            )
        };
        let played = output
            .as_ref()
            .and_then(safe_played_position)
            .or(cached_played);
        PlaybackPosition { submitted, played }
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
        let mut data = lock(&self.shared.data);
        while is_active(data.state) {
            data = wait(&self.shared.changed, data);
        }
        data.state
    }

    fn reap_previous_worker(&self) -> Result<(), PlaybackError> {
        let previous = lock(&self.worker).take();
        if previous.is_some_and(|worker| worker.join().is_err()) {
            return Err(PlaybackError::WorkerPanicked);
        }
        Ok(())
    }
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn worker_entry(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
) {
    let panic_ready = ready.clone();
    let worker_shared = Arc::clone(&shared);
    let worker_output = Arc::clone(&output);
    let worker_cancellation = cancellation.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        worker_loop(shared, generation, source, output, cancellation, ready);
    }));

    if result.is_err() {
        let error = PlaybackError::WorkerPanicked;
        worker_cancellation.cancel();
        fail_session(
            &worker_shared,
            generation,
            error.clone(),
            &worker_cancellation,
            &worker_output,
        );
        let _ = panic_ready.try_send(Err(error));
    }
}

fn worker_loop(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
) {
    let mut decoder = match AudioDecoder::open_with_options(
        source,
        DecoderOptions {
            cancellation: cancellation.clone(),
            ..DecoderOptions::default()
        },
    ) {
        Ok(decoder) => decoder,
        Err(error) => {
            let error = if is_stopped(&shared, generation) {
                PlaybackError::Cancelled
            } else {
                error.into()
            };
            fail_session(&shared, generation, error.clone(), &cancellation, &output);
            let _ = ready.send(Err(error));
            return;
        }
    };

    let info = decoder.info().clone();
    if ready.send(Ok(info)).is_err() {
        cancellation.cancel();
        let _ = safe_cancel(&output);
        return;
    }
    if !wait_for_start(&shared, generation) {
        return;
    }

    let mut pending_frame = None;
    let mut input_finished = false;
    loop {
        match next_worker_action(&shared, generation, &mut pending_frame, input_finished) {
            WorkerAction::Exit => return,
            WorkerAction::Decode => match decoder.next_frame() {
                Ok(Some(frame)) => pending_frame = Some(frame),
                Ok(None) => input_finished = true,
                Err(error) => {
                    if is_stopped(&shared, generation) {
                        return;
                    }
                    fail_session(&shared, generation, error.into(), &cancellation, &output);
                    return;
                }
            },
            WorkerAction::Deliver(frame) => {
                let end_sample = frame
                    .start_sample
                    .saturating_add(frame.samples_per_channel as u64);
                match output.write_frame(frame, &cancellation) {
                    Ok(()) => {
                        let played = safe_played_position(&output);
                        let mut data = lock(&shared.data);
                        if data.generation == generation && !data.stopping {
                            data.submitted_position = duration_from_samples(end_sample);
                            if played.is_some() {
                                data.played_position = played;
                            }
                        }
                    }
                    Err(_) if is_stopped(&shared, generation) => return,
                    Err(error) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
            }
            WorkerAction::Pause => match output.pause(&cancellation) {
                Ok(()) => {
                    let mut data = lock(&shared.data);
                    if data.generation == generation
                        && !data.stopping
                        && data.state == PlaybackState::Pausing
                    {
                        data.state = PlaybackState::Paused;
                    }
                    shared.changed.notify_all();
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
            WorkerAction::Resume => match output.resume(&cancellation) {
                Ok(()) => {
                    let mut data = lock(&shared.data);
                    if data.generation == generation
                        && !data.stopping
                        && data.state == PlaybackState::Resuming
                    {
                        data.state = PlaybackState::Playing;
                    }
                    shared.changed.notify_all();
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
            WorkerAction::Seek(request) => match decoder.seek(request.position) {
                Ok(()) => {
                    pending_frame = None;
                    input_finished = false;
                    match output.flush(request.position, &cancellation) {
                        Ok(()) => {
                            let played = safe_played_position(&output);
                            let mut data = lock(&shared.data);
                            if data.generation == generation && !data.stopping {
                                data.submitted_position = request.position;
                                data.played_position = played;
                            }
                            let _ = request.response.send(Ok(()));
                        }
                        Err(_) if is_stopped(&shared, generation) => {
                            let _ = request.response.send(Err(PlaybackError::Cancelled));
                            return;
                        }
                        Err(error) => {
                            let playback_error = PlaybackError::Output(error);
                            let _ = request.response.send(Err(playback_error.clone()));
                            fail_session(
                                &shared,
                                generation,
                                playback_error,
                                &cancellation,
                                &output,
                            );
                            return;
                        }
                    }
                }
                Err(error) => {
                    if is_stopped(&shared, generation) {
                        let _ = request.response.send(Err(PlaybackError::Cancelled));
                        return;
                    }
                    let playback_error: PlaybackError = error.into();
                    let recoverable = matches!(
                        playback_error,
                        PlaybackError::UnsupportedSeeking(_) | PlaybackError::SeekOutOfRange { .. }
                    );
                    let _ = request.response.send(Err(playback_error.clone()));
                    if !recoverable && !is_stopped(&shared, generation) {
                        fail_session(&shared, generation, playback_error, &cancellation, &output);
                        return;
                    }
                    if is_stopped(&shared, generation) {
                        return;
                    }
                }
            },
            WorkerAction::Finish => match output.finish(&cancellation) {
                Ok(()) => {
                    let played = safe_played_position(&output);
                    let mut data = lock(&shared.data);
                    if data.generation == generation && !data.stopping {
                        data.played_position = played.or(data.played_position);
                        data.state = PlaybackState::Completed;
                    }
                    shared.changed.notify_all();
                    return;
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
        }
    }
}

fn wait_for_start(shared: &SessionShared, generation: u64) -> bool {
    let mut data = lock(&shared.data);
    while data.generation == generation && data.state == PlaybackState::Starting {
        data = wait(&shared.changed, data);
    }
    data.generation == generation && data.state == PlaybackState::Playing
}

fn next_worker_action(
    shared: &SessionShared,
    generation: u64,
    pending_frame: &mut Option<PcmFrame>,
    input_finished: bool,
) -> WorkerAction {
    let mut data = lock(&shared.data);
    loop {
        if data.generation != generation || data.stopping || data.state == PlaybackState::Stopped {
            return WorkerAction::Exit;
        }
        if let Some(request) = data.pending_seek.take() {
            return WorkerAction::Seek(request);
        }
        if data.state == PlaybackState::Pausing {
            return WorkerAction::Pause;
        }
        match data.state {
            PlaybackState::Playing => {
                if let Some(frame) = pending_frame.take() {
                    return WorkerAction::Deliver(frame);
                }
                if input_finished {
                    return WorkerAction::Finish;
                }
                return WorkerAction::Decode;
            }
            PlaybackState::Paused | PlaybackState::Starting => {
                data = wait(&shared.changed, data);
            }
            PlaybackState::Resuming => return WorkerAction::Resume,
            PlaybackState::Stopping => return WorkerAction::Exit,
            PlaybackState::Idle
            | PlaybackState::Pausing
            | PlaybackState::Stopped
            | PlaybackState::Completed
            | PlaybackState::Failed => return WorkerAction::Exit,
        }
    }
}

fn fail_session(
    shared: &SessionShared,
    generation: u64,
    error: PlaybackError,
    cancellation: &CancellationToken,
    output: &Arc<dyn MediaOutput>,
) {
    if is_stopped(shared, generation) {
        return;
    }

    cancellation.cancel();
    let _ = safe_cancel(output);
    let pending_seek = {
        let mut data = lock(&shared.data);
        if data.generation != generation
            || matches!(
                data.state,
                PlaybackState::Stopped | PlaybackState::Completed
            )
        {
            return;
        }
        data.state = PlaybackState::Failed;
        data.last_error = Some(error.clone());
        let pending_seek = data.pending_seek.take();
        shared.changed.notify_all();
        pending_seek
    };
    if let Some(request) = pending_seek {
        let _ = request.response.send(Err(error));
    }
}

fn is_stopped(shared: &SessionShared, generation: u64) -> bool {
    let data = lock(&shared.data);
    data.generation != generation || data.stopping || data.state == PlaybackState::Stopped
}

fn is_active(state: PlaybackState) -> bool {
    matches!(
        state,
        PlaybackState::Starting
            | PlaybackState::Playing
            | PlaybackState::Pausing
            | PlaybackState::Resuming
            | PlaybackState::Paused
            | PlaybackState::Stopping
    )
}

fn duration_from_samples(samples: u64) -> Duration {
    let rate = u64::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ);
    let seconds = samples / rate;
    let remainder = samples % rate;
    let nanoseconds = (u128::from(remainder) * 1_000_000_000 / u128::from(rate)) as u32;
    Duration::new(seconds, nanoseconds)
}

fn normalize_position(position: Duration) -> Result<Duration, PlaybackError> {
    let sample_position =
        position.as_nanos() * u128::from(crate::decoder::OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    let sample_position = u64::try_from(sample_position).map_err(|_| {
        PlaybackError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
    })?;
    Ok(duration_from_samples(sample_position))
}

fn safe_cancel(output: &Arc<dyn MediaOutput>) -> Result<(), ()> {
    catch_unwind(AssertUnwindSafe(|| output.cancel())).map_err(|_| ())
}

fn safe_played_position(output: &Arc<dyn MediaOutput>) -> Option<Duration> {
    catch_unwind(AssertUnwindSafe(|| output.played_position()))
        .ok()
        .flatten()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        PlaybackError, PlaybackSession, PlaybackState, SessionData, SessionShared, lock, wait,
    };
    use crate::decoder::PcmFrame;
    use crate::output::{MediaOutput, NullOutput, NullOutputStats, OutputError};
    use crate::source::{CancellationToken, InputSource};

    static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);
    const TEST_TIMEOUT: Duration = Duration::from_secs(3);

    struct TempWave {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TempWave {
        fn constant(sample_count: usize, sample: i16) -> Self {
            Self::from_samples(vec![sample; sample_count])
        }

        fn from_samples(samples: Vec<i16>) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let directory =
                std::env::temp_dir().join(format!("remedia-playback-{}-{id}", std::process::id()));
            fs::create_dir_all(&directory).expect("create playback test directory");
            let path = directory.join("source.wav");
            fs::write(&path, pcm_wave(&samples)).expect("write playback fixture");
            Self { directory, path }
        }

        fn source(&self) -> InputSource {
            InputSource::file(&self.path)
        }
    }

    impl Drop for TempWave {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[derive(Clone)]
    struct GateOutput(Arc<GateShared>);

    #[derive(Clone)]
    struct GateHandle(Arc<GateShared>);

    struct GateShared {
        data: Mutex<GateData>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct GateData {
        permits: usize,
        write_calls: usize,
        accepted: Vec<(u64, f32)>,
        flushes: Vec<Duration>,
        position: Duration,
        cancelled: bool,
        finished: bool,
    }

    impl GateOutput {
        fn new() -> (Self, GateHandle) {
            let shared = Arc::new(GateShared {
                data: Mutex::new(GateData::default()),
                changed: Condvar::new(),
            });
            (Self(Arc::clone(&shared)), GateHandle(shared))
        }
    }

    impl GateHandle {
        fn release(&self, count: usize) {
            let mut data = lock(&self.0.data);
            data.permits = data.permits.saturating_add(count);
            self.0.changed.notify_all();
        }

        fn wait_for_calls(&self, count: usize) -> bool {
            let data = lock(&self.0.data);
            let (data, _) = self
                .0
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.write_calls < count && !data.cancelled
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.write_calls >= count
        }

        fn wait_for_accepted(&self, count: usize) -> bool {
            let data = lock(&self.0.data);
            let (data, _) = self
                .0
                .changed
                .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                    data.accepted.len() < count && !data.cancelled
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.accepted.len() >= count
        }

        fn snapshot(&self) -> GateSnapshot {
            let data = lock(&self.0.data);
            GateSnapshot {
                write_calls: data.write_calls,
                accepted: data.accepted.clone(),
                flushes: data.flushes.clone(),
                cancelled: data.cancelled,
                finished: data.finished,
            }
        }
    }

    struct GateSnapshot {
        write_calls: usize,
        accepted: Vec<(u64, f32)>,
        flushes: Vec<Duration>,
        cancelled: bool,
        finished: bool,
    }

    impl MediaOutput for GateOutput {
        fn write_frame(
            &self,
            frame: PcmFrame,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            let mut data = lock(&self.0.data);
            data.write_calls += 1;
            self.0.changed.notify_all();
            while data.permits == 0 && !data.cancelled && !cancellation.is_cancelled() {
                data = wait(&self.0.changed, data);
            }
            if data.cancelled || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            data.permits -= 1;
            let first_sample = frame.samples.first().copied().unwrap_or_default();
            data.position = samples_to_duration(
                frame
                    .start_sample
                    .saturating_add(frame.samples_per_channel as u64),
            );
            data.accepted.push((frame.start_sample, first_sample));
            self.0.changed.notify_all();
            Ok(())
        }

        fn flush(
            &self,
            position: Duration,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let mut data = lock(&self.0.data);
            if data.cancelled {
                return Err(OutputError::Cancelled);
            }
            data.position = position;
            data.flushes.push(position);
            self.0.changed.notify_all();
            Ok(())
        }

        fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let mut data = lock(&self.0.data);
            data.finished = true;
            self.0.changed.notify_all();
            Ok(())
        }

        fn cancel(&self) {
            let mut data = lock(&self.0.data);
            data.cancelled = true;
            self.0.changed.notify_all();
        }

        fn played_position(&self) -> Option<Duration> {
            Some(lock(&self.0.data).position)
        }
    }

    struct FailingOutput(Arc<AtomicBool>);

    impl MediaOutput for FailingOutput {
        fn write_frame(
            &self,
            _frame: PcmFrame,
            _cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            Err(OutputError::new("controlled output failure"))
        }

        fn flush(
            &self,
            _position: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            Ok(())
        }

        fn finish(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
            Ok(())
        }

        fn cancel(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn null_output_reports_natural_completion_and_position() {
        let source = TempWave::constant(4800, 4096);
        let session = PlaybackSession::new();
        let output = NullOutput::new();
        let stats = output.stats_handle();

        let info = session.play(source.source(), output).unwrap();
        assert_eq!(info.duration, Some(Duration::from_millis(100)));
        assert_eq!(session.wait(), PlaybackState::Completed);

        let output_stats: NullOutputStats = stats.snapshot();
        assert!(output_stats.frames_received > 0);
        assert_eq!(output_stats.samples_received, 9600);
        assert_eq!(output_stats.sample_frames_received, 4800);
        assert_eq!(output_stats.duration, Duration::from_millis(100));
        assert!(output_stats.finished_successfully);
        assert_eq!(session.duration(), Some(Duration::from_millis(100)));
        assert_eq!(session.position().submitted, Duration::from_millis(100));
        assert_eq!(session.position().played, Some(Duration::from_millis(100)));
        session.stop().unwrap();
        assert_eq!(session.state(), PlaybackState::Completed);
    }

    #[test]
    fn pause_resume_stop_and_repeated_operation_transitions_are_typed() {
        let source = TempWave::constant(96_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        assert!(matches!(
            session.pause(),
            Err(PlaybackError::InvalidState { .. })
        ));
        assert!(matches!(
            session.resume(),
            Err(PlaybackError::InvalidState { .. })
        ));
        session.stop().unwrap();
        session.stop().unwrap();
        assert_eq!(session.state(), PlaybackState::Stopped);

        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(matches!(
            session.play(source.source(), NullOutput::new()),
            Err(PlaybackError::InvalidState { .. })
        ));
        pause_at_first_blocked_frame(&session, &gate);
        session.pause().unwrap();
        assert_eq!(session.state(), PlaybackState::Paused);
        session.resume().unwrap();
        session.resume().unwrap();
        assert!(gate.wait_for_calls(2));
        session.stop().unwrap();
        session.stop().unwrap();
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn stop_during_paused_playback_releases_the_worker() {
        let source = TempWave::constant(48_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        pause_at_first_blocked_frame(&session, &gate);

        session.stop().unwrap();

        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn concurrent_stop_calls_wait_for_cleanup_and_both_succeed() {
        let source = TempWave::constant(48_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let barrier = Arc::new(Barrier::new(3));
        let first_session = Arc::clone(&session);
        let first_barrier = Arc::clone(&barrier);
        let first_stop = thread::spawn(move || {
            first_barrier.wait();
            first_session.stop()
        });
        let second_session = Arc::clone(&session);
        let second_barrier = Arc::clone(&barrier);
        let second_stop = thread::spawn(move || {
            second_barrier.wait();
            second_session.stop()
        });

        barrier.wait();
        assert!(first_stop.join().unwrap().is_ok());
        assert!(second_stop.join().unwrap().is_ok());
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(gate.snapshot().cancelled);
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn blocked_output_applies_backpressure_without_a_session_audio_queue() {
        let source = TempWave::constant(96_000, 4096);
        let session = PlaybackSession::new();
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let blocked = gate.snapshot();
        assert_eq!(blocked.write_calls, 1);
        assert!(blocked.accepted.is_empty());
        assert_eq!(session.position().submitted, Duration::ZERO);

        session.stop().unwrap();
        let stopped = gate.snapshot();
        assert_eq!(stopped.write_calls, 1);
        assert!(stopped.cancelled);
    }

    #[test]
    fn seeking_while_paused_flushes_output_and_discards_stale_frames() {
        let source = TempWave::from_samples(
            (0..96_000)
                .map(|index| if index < 48_000 { 1200 } else { 12_000 })
                .collect(),
        );
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        pause_at_first_blocked_frame(&session, &gate);

        let target = Duration::from_secs(1);
        session.seek(target).unwrap();
        assert_eq!(session.position().submitted, target);
        assert_eq!(gate.snapshot().flushes, vec![target]);
        let accepted_before_seek_resume = gate.snapshot().accepted.len();

        session.resume().unwrap();
        assert!(gate.wait_for_calls(accepted_before_seek_resume + 1));
        gate.release(1);
        assert!(gate.wait_for_accepted(accepted_before_seek_resume + 1));
        let snapshot = gate.snapshot();
        let (start_sample, first_sample) = snapshot.accepted[accepted_before_seek_resume];
        assert!(start_sample >= 48_000);
        assert!(
            first_sample > 0.2,
            "post-seek audio should come from the second half"
        );

        session.stop().unwrap();
    }

    #[test]
    fn decoder_and_output_failures_reach_the_session() {
        let invalid_directory =
            std::env::temp_dir().join(format!("remedia-playback-invalid-{}", std::process::id()));
        fs::create_dir_all(&invalid_directory).unwrap();
        let invalid_path = invalid_directory.join("invalid.bin");
        fs::write(&invalid_path, b"not a media file").unwrap();

        let decoder_session = PlaybackSession::new();
        let (decoder_error_output, decoder_output_handle) = GateOutput::new();
        assert!(matches!(
            decoder_session.play(InputSource::file(&invalid_path), decoder_error_output),
            Err(PlaybackError::Decoder(_))
        ));
        assert_eq!(decoder_session.state(), PlaybackState::Failed);
        assert!(decoder_output_handle.snapshot().cancelled);
        assert!(matches!(
            decoder_session.last_error(),
            Some(PlaybackError::Decoder(_))
        ));
        decoder_session.stop().unwrap();
        let _ = fs::remove_dir_all(&invalid_directory);

        let source = TempWave::constant(48_000, 4096);
        let output_cancelled = Arc::new(AtomicBool::new(false));
        let output_session = PlaybackSession::new();
        output_session
            .play(
                source.source(),
                FailingOutput(Arc::clone(&output_cancelled)),
            )
            .unwrap();
        assert_eq!(output_session.wait(), PlaybackState::Failed);
        assert!(matches!(
            output_session.last_error(),
            Some(PlaybackError::Output(_))
        ));
        assert!(output_cancelled.load(Ordering::Acquire));
        output_session.stop().unwrap();
    }

    #[test]
    fn separate_sessions_progress_independently_under_backpressure() {
        let source_a = TempWave::constant(24_000, 3000);
        let source_b = TempWave::constant(24_000, 6000);
        let session_a = PlaybackSession::new();
        let session_b = PlaybackSession::new();
        let (output_a, gate_a) = GateOutput::new();
        let (output_b, gate_b) = GateOutput::new();

        session_a.play(source_a.source(), output_a).unwrap();
        session_b.play(source_b.source(), output_b).unwrap();
        assert!(gate_a.wait_for_calls(1));
        assert!(gate_b.wait_for_calls(1));
        assert!(gate_a.snapshot().accepted.is_empty());
        assert!(gate_b.snapshot().accepted.is_empty());

        gate_a.release(usize::MAX);
        assert_eq!(session_a.wait(), PlaybackState::Completed);
        assert!(gate_a.snapshot().finished);
        assert_eq!(session_b.state(), PlaybackState::Playing);
        assert_eq!(gate_b.snapshot().write_calls, 1);
        assert!(gate_b.snapshot().accepted.is_empty());
        assert_eq!(session_b.position().submitted, Duration::ZERO);

        gate_b.release(usize::MAX);
        assert_eq!(session_b.wait(), PlaybackState::Completed);
        session_a.stop().unwrap();
        session_b.stop().unwrap();
    }

    #[test]
    fn stop_and_seek_can_race_without_deadlock_or_stale_delivery() {
        let source = TempWave::constant(96_000, 4096);
        let session = Arc::new(PlaybackSession::new());
        let (output, gate) = GateOutput::new();
        session.play(source.source(), output).unwrap();
        assert!(gate.wait_for_calls(1));

        let seeking_session = Arc::clone(&session);
        let seek_thread = thread::spawn(move || seeking_session.seek(Duration::from_secs(1)));
        assert!(wait_for_pending_seek(&session));
        session.stop().unwrap();
        assert!(matches!(
            seek_thread.join().unwrap(),
            Err(PlaybackError::Cancelled)
        ));

        let snapshot = gate.snapshot();
        assert_eq!(session.state(), PlaybackState::Stopped);
        assert!(snapshot.cancelled);
        assert!(snapshot.accepted.is_empty());
        assert!(lock(&session.worker).is_none());
    }

    #[test]
    fn stop_then_play_replaces_a_source_without_old_frames_after_stop() {
        let old_source = TempWave::constant(96_000, 1000);
        let new_source = TempWave::constant(4800, 12_000);
        let session = PlaybackSession::new();
        let (old_output, old_gate) = GateOutput::new();
        session.play(old_source.source(), old_output).unwrap();
        assert!(old_gate.wait_for_calls(1));
        old_gate.release(1);
        assert!(old_gate.wait_for_accepted(1));
        assert!(old_gate.wait_for_calls(2));

        session.stop().unwrap();
        let old_snapshot_at_stop = old_gate.snapshot();
        assert_eq!(old_snapshot_at_stop.accepted.len(), 1);
        assert!(old_snapshot_at_stop.cancelled);

        let output = NullOutput::new();
        let stats = output.stats_handle();
        session.play(new_source.source(), output).unwrap();
        assert_eq!(session.wait(), PlaybackState::Completed);
        assert_eq!(stats.snapshot().sample_frames_received, 4800);
        assert_eq!(
            old_gate.snapshot().accepted.len(),
            old_snapshot_at_stop.accepted.len()
        );
        session.stop().unwrap();
    }

    fn pause_at_first_blocked_frame(session: &Arc<PlaybackSession>, gate: &GateHandle) {
        assert!(gate.wait_for_calls(1));
        let pause_session = Arc::clone(session);
        let pause_thread = thread::spawn(move || pause_session.pause());
        assert!(wait_for_state(session, PlaybackState::Pausing));
        gate.release(1);
        assert!(pause_thread.join().unwrap().is_ok());
        assert_eq!(session.state(), PlaybackState::Paused);
    }

    fn wait_for_state(session: &PlaybackSession, expected: PlaybackState) -> bool {
        wait_for_condition(&session.shared, TEST_TIMEOUT, |data| data.state == expected)
    }

    fn wait_for_pending_seek(session: &PlaybackSession) -> bool {
        wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
            data.pending_seek.is_some()
        })
    }

    fn wait_for_condition(
        shared: &SessionShared,
        timeout: Duration,
        condition: impl Fn(&SessionData) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut data = lock(&shared.data);
        while !condition(&data) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let result = shared.changed.wait_timeout(data, remaining);
            match result {
                Ok((next, wait_result)) => {
                    data = next;
                    if wait_result.timed_out() && !condition(&data) {
                        return false;
                    }
                }
                Err(poisoned) => data = poisoned.into_inner().0,
            }
        }
        true
    }

    fn pcm_wave(samples: &[i16]) -> Vec<u8> {
        let channels = 1_u16;
        let sample_rate = 48_000_u32;
        let bytes_per_sample = 2_u16;
        let block_align = channels * bytes_per_sample;
        let data_len = samples.len() * usize::from(bytes_per_sample);
        let mut bytes = Vec::with_capacity(44 + data_len);
        bytes.extend_from_slice(b"RIFF");
        push_u32(&mut bytes, (36 + data_len) as u32);
        bytes.extend_from_slice(b"WAVEfmt ");
        push_u32(&mut bytes, 16);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, channels);
        push_u32(&mut bytes, sample_rate);
        push_u32(&mut bytes, sample_rate * u32::from(block_align));
        push_u16(&mut bytes, block_align);
        push_u16(&mut bytes, 16);
        bytes.extend_from_slice(b"data");
        push_u32(&mut bytes, data_len as u32);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn samples_to_duration(samples: u64) -> Duration {
        let seconds = samples / 48_000;
        let remainder = samples % 48_000;
        let nanoseconds = (u128::from(remainder) * 1_000_000_000 / 48_000) as u32;
        Duration::new(seconds, nanoseconds)
    }
}
