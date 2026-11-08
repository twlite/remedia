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
    shared: Arc<SessionShared>,
    expires_at_unix_ms: Option<u64>,
    cancellation: CancellationToken,
    info: Mutex<Option<AudioStreamInfo>>,
    error: Mutex<Option<String>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl PreparedStage {
    fn start(
        source: InputSource,
        identity: StageIdentity,
        shared: Arc<SessionShared>,
        expires_at_unix_ms: Option<u64>,
    ) -> Result<Arc<Self>, PlaybackError> {
        let stage = Arc::new(Self {
            identity,
            source: source.clone(),
            shared,
            expires_at_unix_ms,
            buffer: Arc::new(StagedPcmBuffer::with_capacity_ms(2_000).expect("valid buffer")),
            cancellation: CancellationToken::new(),
            info: Mutex::new(None),
            error: Mutex::new(None),
            worker: Mutex::new(None),
        });
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
            while !self.cancellation.is_cancelled() {
                match decoder.next_frame()? {
                    Some(frame) => {
                        if self.buffer.push(&frame.samples).is_err() {
                            return Ok(());
                        }
                        if !ready_sent && self.buffer.buffered_samples() >= 48_000 {
                            ready_sent = true;
                            self.shared.publish(CrossfadeEvent::StageReady {
                                identity: self.identity.clone(),
                            });
                        }
                    }
                    None => {
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
            let next = match PreparedStage::start(
                source,
                identity,
                Arc::clone(&self.shared),
                expires_at_unix_ms,
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
        let old = {
            let mut slot = lock(&self.stage);
            if let Some(stage) = slot.as_ref() {
                stage.cancellation.cancel();
                stage.buffer.cancel();
            }
            slot.take()
        };
        if let Some(old) = old {
            self.shared.publish(CrossfadeEvent::StageInvalidated {
                identity: old.identity.clone(),
            });
            old.cancel_and_join();
        }
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
