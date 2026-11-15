use super::*;
use crate::crossfade::StagedPcmBuffer;

/// A unique upcoming queue occurrence, independent of the source URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageIdentity {
    pub expected_generation: u64,
    pub entry_id: String,
    pub queue_revision: u64,
    pub source_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageState {
    Preparing,
    Ready,
    Failed,
}

#[derive(Clone, Debug)]
pub struct StageSnapshot {
    pub identity: StageIdentity,
    pub state: StageState,
    pub info: Option<AudioStreamInfo>,
    pub buffered_samples: usize,
    pub error: Option<String>,
}

pub(super) struct PreparedStage {
    pub(super) identity: StageIdentity,
    pub(super) buffer: Arc<StagedPcmBuffer>,
    pub(super) source: InputSource,
    /// How to obtain the staged track again if it becomes the active source and is then asked to
    /// seek in a way its decoder cannot.
    pub(super) reopen: fallback::Reopen,
    pub(super) trimmed_samples: std::sync::atomic::AtomicU64,
    shared: Arc<SessionShared>,
    expires_at_unix_ms: Option<u64>,
    cancellation: CancellationToken,
    info: Mutex<Option<AudioStreamInfo>>,
    error: Mutex<Option<String>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// The staged decoder's input can reposition itself. `true` until its input proves otherwise,
    /// which is what a seek after promotion needs to know to choose a fallback replacement.
    random_access: std::sync::atomic::AtomicBool,
    /// The staged stream can forward Opus packets and its input is local, so its decoder may
    /// move to the playback worker (set once its input is open).
    passthrough_eligible: std::sync::atomic::AtomicBool,
    /// The promoted source asked this stage to park its decoder instead of decoding further.
    handoff_requested: std::sync::atomic::AtomicBool,
    handed_off: std::sync::atomic::AtomicBool,
    handoff: Mutex<Option<Box<AudioDecoder>>>,
    /// Cancels this stage's decoder when the owning playback is cancelled. Held so the weak
    /// registration with the playback token stays live for as long as the decoder can run.
    session_link: Mutex<Option<Arc<dyn crate::source::CancellationWake>>>,
}

/// Forwards cancellation of a playback to a stage whose decoder may outlive its stage thread.
struct StageCancelLink(CancellationToken);

impl crate::source::CancellationWake for StageCancelLink {
    fn wake_for_cancellation(&self) {
        self.0.cancel();
    }
}

impl PreparedStage {
    fn start(
        source: InputSource,
        reopener: Option<Arc<dyn SourceReopener>>,
        identity: StageIdentity,
        shared: Arc<SessionShared>,
        expires_at_unix_ms: Option<u64>,
        playback_cancellation: Option<CancellationToken>,
    ) -> Result<Arc<Self>, PlaybackError> {
        let stage = Arc::new(Self {
            identity,
            reopen: fallback::Reopen::new(&source, reopener),
            source: source.clone(),
            trimmed_samples: std::sync::atomic::AtomicU64::new(0),
            shared,
            expires_at_unix_ms,
            buffer: Arc::new(StagedPcmBuffer::with_capacity_ms(2_000).expect("valid buffer")),
            cancellation: CancellationToken::new(),
            info: Mutex::new(None),
            error: Mutex::new(None),
            worker: Mutex::new(None),
            random_access: std::sync::atomic::AtomicBool::new(true),
            passthrough_eligible: std::sync::atomic::AtomicBool::new(false),
            handoff_requested: std::sync::atomic::AtomicBool::new(false),
            handed_off: std::sync::atomic::AtomicBool::new(false),
            handoff: Mutex::new(None),
            session_link: Mutex::new(None),
        });
        if let Some(playback) = playback_cancellation {
            let link: Arc<dyn crate::source::CancellationWake> =
                Arc::new(StageCancelLink(stage.cancellation.clone()));
            playback.register_waker(Arc::clone(&link));
            *lock(&stage.session_link) = Some(link);
        }
        let worker_stage = Arc::clone(&stage);
        let handle = thread::Builder::new()
            .name("remedia-stage-next".into())
            .spawn(move || worker_stage.run(source))
            .map_err(|error| PlaybackError::WorkerInitialization(Arc::new(error)))?;
        *lock(&stage.worker) = Some(handle);
        Ok(stage)
    }

    fn run(&self, source: InputSource) {
        let mut ready_sent = false;
        let result = (|| -> Result<(), PlaybackError> {
            let mut decoder = AudioDecoder::open_with_options(
                source,
                DecoderOptions {
                    cancellation: self.cancellation.clone(),
                    ..DecoderOptions::default()
                },
            )?;
            *lock(&self.info) = Some(decoder.info().clone());
            self.random_access.store(
                decoder.can_seek_in_place(),
                std::sync::atomic::Ordering::Release,
            );
            // Handing the decoder to the playback worker removes the staged PCM cushion that
            // absorbs a stalled remote read, so only inputs that cannot stall on a remote peer
            // qualify. Network and producer-fed streams keep their buffered stage.
            let eligible = decoder.opus_passthrough_rejection().is_none()
                && decoder.opus_pre_skip() == crate::decoder::OPUS_ENCODER_LOOKAHEAD
                && matches!(self.source, InputSource::File(_) | InputSource::Bytes(_));
            self.passthrough_eligible
                .store(eligible, std::sync::atomic::Ordering::Release);
            let mut prefix = Vec::new();
            let mut prefix_done = false;
            let mut prefix_checked = 0;
            while !self.cancellation.is_cancelled() {
                if prefix_done
                    && self
                        .handoff_requested
                        .load(std::sync::atomic::Ordering::Acquire)
                    && !decoder.has_pending_frames()
                {
                    // Every decoded frame has been pushed, so the decoder sits on a frame
                    // boundary. Park it for the promoted source and let it drain the buffer.
                    *lock(&self.handoff) = Some(Box::new(decoder));
                    self.handed_off
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.buffer.finish();
                    return Ok(());
                }
                match decoder.next_frame()? {
                    Some(frame) => {
                        let samples =
                            if !prefix_done {
                                prefix.extend_from_slice(&frame.samples);
                                // At most two seconds. Any non-padding sample ends
                                // inspection; quiet music is returned unchanged.
                                let padding =
                                    prefix[prefix_checked..].as_chunks::<960>().0.iter().all(
                                        |hop| {
                                            hop.iter().all(|s| s.is_finite() && s.abs() <= 0.000_03)
                                        },
                                    );
                                prefix_checked = prefix.len() / 960 * 960;
                                if prefix.len() < 2 * 96_000 && padding {
                                    continue;
                                }
                                prefix_done = true;
                                let mut trim = crate::crossfade::scheduling::leading_trim(&prefix);
                                if eligible {
                                    // Keep the incoming source's 20 ms packet grid aligned with
                                    // the output's, so it can resume forwarding packets. At most
                                    // 19.98 ms of near-silent lead-in is retained.
                                    trim -= trim % 960;
                                }
                                self.trimmed_samples
                                    .store(trim as u64, std::sync::atomic::Ordering::Release);
                                prefix.drain(..trim * 2);
                                std::mem::take(&mut prefix)
                            } else {
                                frame.samples
                            };
                        for block in samples.chunks(1920) {
                            if self.buffer.push(block).is_err() {
                                return Ok(());
                            }
                            if !ready_sent && self.buffer.buffered_samples() >= 48_000 {
                                ready_sent = true;
                                self.shared.publish(CrossfadeEvent::StageReady {
                                    identity: self.identity.clone(),
                                });
                            }
                        }
                    }
                    None => {
                        // No confirmed onset: preserve an all-silent/short input.
                        if !prefix.is_empty() && self.buffer.push(&prefix).is_err() {
                            return Ok(());
                        }
                        self.buffer.finish();
                        if !ready_sent && self.buffer.buffered_samples() > 0 {
                            self.shared.publish(CrossfadeEvent::StageReady {
                                identity: self.identity.clone(),
                            });
                        }
                        return Ok(());
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if !self.cancellation.is_cancelled() {
                self.shared.publish(CrossfadeEvent::StageFailed {
                    identity: self.identity.clone(),
                    reason: error.to_string(),
                });
                *lock(&self.error) = Some(error.to_string());
            }
            self.buffer.cancel();
        }
    }

    pub(super) fn snapshot(&self) -> StageSnapshot {
        let info = lock(&self.info).clone();
        let error = lock(&self.error).clone().or_else(|| {
            self.expires_at_unix_ms
                .filter(|expires| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .is_ok_and(|now| now.as_millis() >= u128::from(*expires))
                })
                .map(|_| "staged source expired before promotion".to_owned())
        });
        let buffered_samples = self.buffer.buffered_samples();
        let ready = info.is_some()
            && (buffered_samples >= 48_000 || (self.buffer.is_finished() && buffered_samples > 0));
        StageSnapshot {
            identity: self.identity.clone(),
            state: if error.is_some() {
                StageState::Failed
            } else if ready {
                StageState::Ready
            } else {
                StageState::Preparing
            },
            info,
            buffered_samples,
            error,
        }
    }

    /// Whether the staged input can reposition itself, as far as is known.
    pub(super) fn random_access(&self) -> bool {
        self.random_access
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn passthrough_eligible(&self) -> bool {
        self.passthrough_eligible
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn request_handoff(&self) {
        self.handoff_requested
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// The decoder was parked and every staged sample has been consumed.
    pub(super) fn handoff_complete(&self) -> bool {
        self.handed_off.load(std::sync::atomic::Ordering::Acquire)
            && self.buffer.buffered_samples() == 0
    }

    pub(super) fn take_handed_off_decoder(&self) -> Option<Box<AudioDecoder>> {
        lock(&self.handoff).take()
    }

    /// Join the stage thread after it parked its decoder. It does not cancel the decoder.
    pub(super) fn join_finished_worker(&self) {
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
    }

    pub(super) fn cancel_link(&self) -> Option<Arc<dyn crate::source::CancellationWake>> {
        lock(&self.session_link).clone()
    }

    pub(super) fn cancel_and_join(&self) {
        self.cancellation.cancel();
        self.buffer.cancel();
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
    }
}

impl PlaybackSession {
    /// Prepare a second source without interrupting the current source. The
    /// source is decoded into bounded PCM, never into the voice output.
    /// Identical stage identities reuse the existing preparation worker.
    pub fn stage_next(
        &self,
        source: InputSource,
        identity: StageIdentity,
    ) -> Result<StageSnapshot, PlaybackError> {
        self.stage_next_with_expiration(source, identity, None)
    }

    /// Stage a source with an optional absolute Unix expiration in milliseconds.
    /// Expired sources are never promoted; an already audible source retains its decoder.
    pub fn stage_next_with_expiration(
        &self,
        source: InputSource,
        identity: StageIdentity,
        expires_at_unix_ms: Option<u64>,
    ) -> Result<StageSnapshot, PlaybackError> {
        self.stage_next_inner(source, None, identity, expires_at_unix_ms)
    }

    /// Stage a source together with the means to obtain it again, for
    /// [fallback seeking](Self::configure_fallback_seeking) after it has been promoted. See
    /// [`play_with_reopener`](Self::play_with_reopener).
    pub fn stage_next_with_reopener(
        &self,
        source: InputSource,
        reopener: Arc<dyn SourceReopener>,
        identity: StageIdentity,
        expires_at_unix_ms: Option<u64>,
    ) -> Result<StageSnapshot, PlaybackError> {
        self.stage_next_inner(source, Some(reopener), identity, expires_at_unix_ms)
    }

    fn stage_next_inner(
        &self,
        source: InputSource,
        reopener: Option<Arc<dyn SourceReopener>>,
        identity: StageIdentity,
        expires_at_unix_ms: Option<u64>,
    ) -> Result<StageSnapshot, PlaybackError> {
        if self.crossfade_config().is_none() {
            return Err(PlaybackError::Cancelled);
        }
        {
            let data = lock(&self.shared.data);
            if !matches!(data.state, PlaybackState::Playing | PlaybackState::Paused)
                || data.source_generation != identity.expected_generation
                || identity.entry_id.trim().is_empty()
            {
                return Err(PlaybackError::Cancelled);
            }
        }
        let (old, next) = {
            let mut slot = lock(&self.stage);
            // Stop first marks the playback state as Stopping and then clears
            // the stage. Recheck under the staging lock so a concurrent
            // stage request cannot publish a decoder after stop clears it.
            {
                let data = lock(&self.shared.data);
                if !matches!(data.state, PlaybackState::Playing | PlaybackState::Paused)
                    || data.source_generation != identity.expected_generation
                {
                    return Err(PlaybackError::Cancelled);
                }
            }
            {
                let data = lock(&self.shared.data);
                if let Some(latest) = &data.latest_stage
                    && latest.expected_generation == identity.expected_generation
                    && (identity.queue_revision < latest.queue_revision
                        || (identity.queue_revision == latest.queue_revision
                            && (identity.entry_id != latest.entry_id
                                || identity.source_revision < latest.source_revision)))
                {
                    return Err(PlaybackError::Cancelled);
                }
            }
            if let Some(old) = slot.as_ref() {
                if old.identity == identity {
                    return Ok(old.snapshot());
                }
                if old.identity.expected_generation == identity.expected_generation
                    && (identity.queue_revision < old.identity.queue_revision
                        || (identity.queue_revision == old.identity.queue_revision
                            && identity.entry_id == old.identity.entry_id
                            && identity.source_revision < old.identity.source_revision))
                {
                    return Err(PlaybackError::Cancelled);
                }
                old.cancellation.cancel();
                old.buffer.cancel();
            }
            let playback_cancellation = lock(&self.shared.data).cancellation.clone();
            let next = match PreparedStage::start(
                source,
                reopener,
                identity,
                Arc::clone(&self.shared),
                expires_at_unix_ms,
                playback_cancellation,
            ) {
                Ok(next) => next,
                Err(error) => {
                    let old = slot.take();
                    drop(slot);
                    if let Some(old) = old {
                        old.cancel_and_join();
                    }
                    return Err(error);
                }
            };
            lock(&self.shared.data).latest_stage = Some(next.identity.clone());
            (slot.replace(Arc::clone(&next)), next)
        };
        if let Some(old) = old {
            self.shared.publish(CrossfadeEvent::StageInvalidated {
                identity: old.identity.clone(),
            });
            old.cancel_and_join();
        }
        Ok(next.snapshot())
    }

    pub fn staged_source(&self) -> Option<StageSnapshot> {
        lock(&self.stage).as_ref().map(|stage| stage.snapshot())
    }

    /// Discard prepared audio when the timeline changes or playback stops.
    pub fn clear_staged_source(&self) {
        clear_stage(&self.shared, &self.stage);
    }
}

/// Cancel the staged source in `slot`, if any, and publish its invalidation.
pub(super) fn clear_stage(shared: &SessionShared, slot: &Mutex<Option<Arc<PreparedStage>>>) {
    let old = {
        let mut slot = lock(slot);
        if let Some(stage) = slot.as_ref() {
            stage.cancellation.cancel();
            stage.buffer.cancel();
        }
        slot.take()
    };
    if let Some(old) = old {
        shared.publish(CrossfadeEvent::StageInvalidated {
            identity: old.identity.clone(),
        });
        old.cancel_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_sessions_cannot_stage() {
        let session = PlaybackSession::new();
        let identity = StageIdentity {
            expected_generation: 1,
            entry_id: "song-a".to_owned(),
            queue_revision: 1,
            source_revision: 1,
        };
        assert!(matches!(
            session.stage_next(InputSource::file("missing.wav"), identity),
            Err(PlaybackError::Cancelled)
        ));
        session.clear_staged_source();
        assert!(session.staged_source().is_none());
    }
}
