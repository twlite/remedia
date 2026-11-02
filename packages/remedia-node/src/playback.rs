use std::sync::Arc;

use napi::bindgen_prelude::Status;
use napi_derive::napi;
use remedia_core::{
    MediaPositionAccuracy, NullOutput, OpusEncoderOutput, PlaybackError, PlaybackSession,
};
use remedia_discord::ConnectionState;

use crate::native::NativeRemedia;
use crate::runtime::{PlaybackEntry, lock, run_blocking};
use crate::types::{NativeMediaSource, NativePlaybackPosition, NativeStreamInfo};
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
        let result = run_blocking(move || match connection {
            Some(connection) => {
                let output =
                    OpusEncoderOutput::new_with_observability(connection.opus_sender(), handle)
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
