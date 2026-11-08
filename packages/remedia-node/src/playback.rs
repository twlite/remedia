use std::sync::Arc;

use napi::bindgen_prelude::Status;
use napi_derive::napi;
use remedia_core::{
    CrossfadeConfig, CrossfadeEvent, MediaPositionAccuracy, NullOutput, OpusEncoderOutput,
    PlaybackError, PlaybackSession, StageIdentity, StageSnapshot, StageState,
};
use remedia_discord::ConnectionState;

use crate::native::NativeRemedia;
use crate::runtime::{PlaybackEntry, lock, run_blocking};
use crate::types::{
    NativeCrossfadeEvent, NativeMediaSource, NativePlaybackPosition, NativeStageSnapshot,
    NativeStreamInfo,
};
use crate::util::{
    duration_from_millis, duration_millis, napi_error, native_stream_info, parse_source,
    playback_error, playback_state_name,
};

#[napi]
impl NativeRemedia {
    /// Create a decoder-only session that consumes frames into a no-op output.
    #[napi]
    pub fn create_playback_session(&self) -> napi::Result<String> {
        self.inner.ensure_open()?;
        let id = self.inner.id();
        let entry = PlaybackEntry::new(
            PlaybackSession::with_observability(self.inner.observability_handle.clone()),
            None,
            None,
        );
        lock(&self.inner.playbacks).insert(id.clone(), entry);
        Ok(id)
    }

    /// Start decoding into the null output or the associated Discord Opus sender.
    #[napi]
    pub async fn play(
        &self,
        playback_id: String,
        source: NativeMediaSource,
    ) -> napi::Result<NativeStreamInfo> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let entry = self.inner.playback(&playback_id)?;
        let input = if source.kind == "stream" {
            let stream_id = source.value.as_deref().ok_or_else(|| {
                napi_error(
                    Status::InvalidArg,
                    "stream media source requires a native byte-stream handle",
                )
            })?;
            self.inner.byte_stream(stream_id)?.source
        } else {
            parse_source(source)?
        };
        let session = Arc::clone(&entry.session);
        let handle = self.inner.observability_handle.clone();
        let connection = match &entry.voice_id {
            Some(voice_id) => {
                let connection = self.inner.voice(voice_id)?.connection;
                if !matches!(
                    connection.state(),
                    ConnectionState::Connected | ConnectionState::Reconnecting
                ) {
                    return Err(napi_error(
                        Status::GenericFailure,
                        "Discord voice transport is not connected",
                    ));
                }
                Some(connection)
            }
            None => None,
        };

        let cancellation = entry.begin_start()?;
        let sender_router = entry.sender_router.clone();
        let result = run_blocking(move || match connection {
            Some(_) => {
                let Some(sender_router) = sender_router else {
                    return Err(PlaybackError::from(remedia_core::OutputError::new(
                        "Discord voice sender is unavailable",
                    )));
                };
                let output =
                    OpusEncoderOutput::new_with_observability(sender_router.sink(), handle)
                        .map_err(PlaybackError::from)?;
                session.play_with_cancellation(input, output, cancellation)
            }
            None => session.play_with_cancellation(input, NullOutput::new(), cancellation),
        })
        .await;
        entry.finish_start();
        let result = result?;
        result
            .map(native_stream_info)
            .map_err(|error| playback_error("could not start playback", error))
    }


    /// Configure the optional continuous PCM transition worker before playback.
    #[napi]
    pub fn playback_configure_crossfade(
        &self,
        playback_id: String,
        enabled: bool,
        duration_ms: u32,
        prepare_before_ms: u32,
    ) -> napi::Result<()> {
        self.inner.ensure_open()?;
        let session = self.inner.playback(&playback_id)?.session;
        let config = if enabled {
            Some(CrossfadeConfig::new(duration_ms, prepare_before_ms).ok_or_else(|| {
                napi_error(Status::InvalidArg, "invalid crossfade duration or preparation lead")
            })?)
        } else {
            None
        };
        session.configure_crossfade(config)
            .map_err(|error| playback_error("could not configure crossfade", error))
    }

    /// Open an independent decoder and stage a queue occurrence without
    /// interrupting current playback.
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub async fn playback_stage_next(
        &self,
        playback_id: String,
        source: NativeMediaSource,
        expected_generation: String,
        entry_id: String,
        queue_revision: String,
        source_revision: String,
    ) -> napi::Result<NativeStageSnapshot> {
        self.inner.ensure_open()?;
        let entry = self.inner.playback(&playback_id)?;
        let media = if source.kind == "stream" {
            let stream_id = source.value.as_deref().ok_or_else(|| {
                napi_error(Status::InvalidArg, "stream source needs a native stream handle")
            })?;
            self.inner.byte_stream(stream_id)?.source
        } else {
            parse_source(source)?
        };
        let generation = expected_generation.parse::<u64>().map_err(|_| {
            napi_error(Status::InvalidArg, "expectedGeneration must be a decimal string")
        })?;
        let revision = queue_revision.parse::<u64>().map_err(|_| {
            napi_error(Status::InvalidArg, "queueRevision must be a decimal string")
        })?;
        let source_revision = source_revision.parse::<u64>().map_err(|_| {
            napi_error(Status::InvalidArg, "sourceRevision must be a decimal string")
        })?;
        let session = Arc::clone(&entry.session);
        let snapshot = run_blocking(move || {
            session.stage_next(media, StageIdentity {
                expected_generation: generation, entry_id,
                queue_revision: revision, source_revision,
            })
        }).await?
          .map_err(|error| playback_error("could not stage source", error))?;
        Ok(native_stage_snapshot(snapshot))
    }

    /// Return the current stage without opening a new decoder.
    #[napi]
    pub fn playback_staged_source(
        &self, playback_id: String,
    ) -> napi::Result<Option<NativeStageSnapshot>> {
        self.inner.ensure_open()?;
        let session = self.inner.playback(&playback_id)?.session;
        Ok(session.staged_source().map(native_stage_snapshot))
    }

    #[napi]
    pub async fn playback_clear_stage(&self, playback_id: String) -> napi::Result<()> {
        let entry = self.inner.playback(&playback_id)?;
        let session = Arc::clone(&entry.session);
        run_blocking(move || session.clear_staged_source()).await?;
        Ok(())
    }

    /// Drain native transition events; embedded wrappers forward these to JS
    /// using the same event names and generation semantics as remote mode.
    #[napi]
    pub fn playback_take_crossfade_events(
        &self, playback_id: String,
    ) -> napi::Result<Vec<NativeCrossfadeEvent>> {
        self.inner.ensure_open()?;
        let session = self.inner.playback(&playback_id)?.session;
        Ok(session.take_crossfade_events().into_iter()
            .map(native_crossfade_event).collect())
    }

    #[napi]
    pub fn playback_source_generation(&self, playback_id: String) -> napi::Result<String> {
        Ok(self.inner.playback(&playback_id)?.session.source_generation().to_string())
    }

    /// Request playback pause at the current output frame boundary.
    #[napi]
    pub async fn pause(&self, playback_id: String) -> napi::Result<()> {
        self.with_playback_operation(&playback_id, |session| session.pause())
            .await
            .map_err(|error| playback_error("could not pause playback", error))
    }

    /// Resume a paused playback session.
    #[napi]
    pub async fn resume(&self, playback_id: String) -> napi::Result<()> {
        self.with_playback_operation(&playback_id, |session| session.resume())
            .await
            .map_err(|error| playback_error("could not resume playback", error))
    }

    /// Seek to a media position in milliseconds.
    #[napi]
    pub async fn seek(&self, playback_id: String, position_ms: f64) -> napi::Result<()> {
        let position = duration_from_millis(position_ms)?;
        self.with_playback_operation(&playback_id, move |session| session.seek(position))
            .await
            .map_err(|error| playback_error("could not seek playback", error))
    }

    /// Validate and replace the trusted native FFmpeg audio filtergraph.
    #[napi]
    pub async fn playback_set_filter_graph(
        &self,
        playback_id: String,
        graph: String,
    ) -> napi::Result<()> {
        self.with_playback_operation(&playback_id, move |session| {
            session.set_filter_graph(Some(&graph))
        })
        .await
        .map_err(|error| playback_error("could not set audio filtergraph", error))
    }

    /// Clear the current native audio filtergraph.
    #[napi]
    pub async fn playback_clear_filter_graph(&self, playback_id: String) -> napi::Result<()> {
        self.with_playback_operation(&playback_id, |session| session.set_filter_graph(None))
            .await
            .map_err(|error| playback_error("could not clear audio filtergraph", error))
    }

    /// Current configured raw FFmpeg filtergraph, if any.
    #[napi]
    pub fn playback_filter_graph(&self, playback_id: String) -> napi::Result<Option<String>> {
        Ok(self.inner.playback(&playback_id)?.session.filter_graph())
    }

    /// Set normalized linear master gain in the inclusive range 0.0 through 2.0.
    #[napi]
    pub async fn playback_set_volume(&self, playback_id: String, gain: f64) -> napi::Result<()> {
        if !gain.is_finite() || !(0.0..=2.0).contains(&gain) {
            return Err(napi_error(
                Status::InvalidArg,
                "master volume must be a finite linear gain between 0.0 and 2.0",
            ));
        }
        let gain = gain as f32;
        self.with_playback_operation(&playback_id, move |session| session.set_volume(gain))
            .await
            .map_err(|error| playback_error("could not set master volume", error))
    }

    /// Current normalized linear master gain.
    #[napi]
    pub fn playback_volume(&self, playback_id: String) -> napi::Result<f64> {
        Ok(f64::from(
            self.inner.playback(&playback_id)?.session.volume(),
        ))
    }

    /// Validate a filtergraph before a Discord voice session has a native playback object.
    #[napi]
    pub async fn validate_filter_graph(&self, graph: String) -> napi::Result<()> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        run_blocking(move || PlaybackSession::validate_filter_graph(&graph))
            .await?
            .map_err(|error| {
                napi_error(
                    Status::InvalidArg,
                    format!("invalid audio filtergraph: {error}"),
                )
            })
    }

    /// Stop playback without disconnecting its Discord voice transport.
    #[napi]
    pub async fn stop(&self, playback_id: String) -> napi::Result<()> {
        self.inner.playback(&playback_id)?.cancel_start();
        self.with_playback_operation(&playback_id, |session| session.stop())
            .await
            .map_err(|error| playback_error("could not stop playback", error))
    }

    /// Capture the source generation before an asynchronous wait is scheduled.
    #[napi]
    pub fn playback_generation(&self, playback_id: String) -> napi::Result<String> {
        Ok(self
            .inner
            .playback(&playback_id)?
            .session
            .completion_generation()
            .to_string())
    }

    /// Wait for a generation captured synchronously by the JavaScript wrapper.
    #[napi]
    pub async fn wait(&self, playback_id: String, generation: String) -> napi::Result<String> {
        self.inner.ensure_open()?;
        let generation = generation
            .parse::<u64>()
            .map_err(|_| napi_error(Status::InvalidArg, "invalid playback generation"))?;
        let waiter = self
            .inner
            .playback(&playback_id)?
            .session
            .completion_for_generation(generation);
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let state = run_blocking(move || waiter.wait()).await?;
        Ok(playback_state_name(state).to_owned())
    }

    /// Current playback lifecycle state.
    #[napi]
    pub fn playback_state(&self, playback_id: String) -> napi::Result<String> {
        Ok(playback_state_name(self.inner.playback(&playback_id)?.session.state()).to_owned())
    }

    /// Source media, filtered output submission, and output-reported played positions.
    #[napi]
    pub fn playback_position(&self, playback_id: String) -> napi::Result<NativePlaybackPosition> {
        let position = self.inner.playback(&playback_id)?.session.position();
        Ok(NativePlaybackPosition {
            media_ms: position.media.map(|media| duration_millis(media.position)),
            media_position_accuracy: position.media.map(|media| match media.accuracy {
                MediaPositionAccuracy::Timestamp => "timestamp".to_owned(),
                MediaPositionAccuracy::Estimated => "estimated".to_owned(),
            }),
            submitted_ms: duration_millis(position.submitted),
            played_ms: position.played.map(duration_millis),
        })
    }

    /// Known media duration in milliseconds.
    #[napi]
    pub fn playback_duration_ms(&self, playback_id: String) -> napi::Result<Option<f64>> {
        Ok(self
            .inner
            .playback(&playback_id)?
            .session
            .duration()
            .map(duration_millis))
    }

    /// Latest decoder, resampler, output, or worker failure.
    #[napi]
    pub fn playback_error(&self, playback_id: String) -> napi::Result<Option<String>> {
        Ok(self
            .inner
            .playback(&playback_id)?
            .session
            .last_error()
            .map(|error| error.to_string()))
    }

    /// Stop and forget a standalone playback session. Voice sessions remain connected.
    #[napi]
    pub async fn dispose_playback_session(&self, playback_id: String) -> napi::Result<()> {
        let _operation = self.inner.operations.read().await;
        if self.inner.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        let entry = self.inner.playback(&playback_id)?;
        entry.mark_disposed();
        let session = Arc::clone(&entry.session);
        let stop_result = run_blocking(move || session.stop()).await?;
        lock(&self.inner.playbacks).remove(&playback_id);
        stop_result.map_err(|error| playback_error("could not dispose playback session", error))
    }
}

fn native_stage_snapshot(value: StageSnapshot) -> NativeStageSnapshot {
    NativeStageSnapshot {
        expected_generation: value.identity.expected_generation.to_string(),
        entry_id: value.identity.entry_id,
        queue_revision: value.identity.queue_revision.to_string(),
        source_revision: value.identity.source_revision.to_string(),
        state: match value.state {
            StageState::Preparing => "preparing",
            StageState::Ready => "ready",
            StageState::Failed => "failed",
        }.to_owned(),
        buffered_ms: (value.buffered_samples as f64) / 48.0,
        info: value.info.map(native_stream_info),
        error: value.error,
    }
}

fn native_crossfade_event(event: CrossfadeEvent) -> NativeCrossfadeEvent {
    let mut value = NativeCrossfadeEvent {
        kind: String::new(),
        generation: String::new(),
        previous_generation: None,
        entry_id: None,
        queue_revision: None,
        source_revision: None,
        remaining_ms: None,
        reason: None,
        info: None,
    };
    match event {
        CrossfadeEvent::PrepareRequested { generation, remaining_ms } => {
            value.kind = "prepare_requested".into();
            value.generation = generation.to_string();
            value.remaining_ms = Some(remaining_ms as f64);
        }
        CrossfadeEvent::StageReady { identity } => {
            value.kind = "stage_ready".into();
            value.generation = identity.expected_generation.to_string();
            fill_crossfade_identity(&mut value, identity);
        }
        CrossfadeEvent::StageFailed { identity, reason } => {
            value.kind = "stage_failed".into();
            value.generation = identity.expected_generation.to_string();
            value.reason = Some(reason);
            fill_crossfade_identity(&mut value, identity);
        }
        CrossfadeEvent::StageInvalidated { identity } => {
            value.kind = "stage_invalidated".into();
            value.generation = identity.expected_generation.to_string();
            fill_crossfade_identity(&mut value, identity);
        }
        CrossfadeEvent::TransitionStarted { outgoing_generation, incoming_generation, identity, info } => {
            value.kind = "transition_started".into();
            value.generation = incoming_generation.to_string();
            value.previous_generation = Some(outgoing_generation.to_string());
            value.info = Some(native_stream_info(info));
            fill_crossfade_identity(&mut value, identity);
        }
        CrossfadeEvent::TransitionCompleted { outgoing_generation, incoming_generation, identity } => {
            value.kind = "transition_completed".into();
            value.generation = outgoing_generation.to_string();
            value.previous_generation = Some(incoming_generation.to_string());
            fill_crossfade_identity(&mut value, identity);
        }
        CrossfadeEvent::TransitionCancelled { outgoing_generation, incoming_generation, identity } => {
            value.kind = "transition_cancelled".into();
            value.generation = outgoing_generation.to_string();
            value.previous_generation = Some(incoming_generation.to_string());
            fill_crossfade_identity(&mut value, identity);
        }
    }
    value
}

fn fill_crossfade_identity(event: &mut NativeCrossfadeEvent, identity: StageIdentity) {
    event.entry_id = Some(identity.entry_id);
    event.queue_revision = Some(identity.queue_revision.to_string());
    event.source_revision = Some(identity.source_revision.to_string());
}
