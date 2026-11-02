use std::sync::Arc;

use napi::bindgen_prelude::Status;
use napi_derive::napi;
use remedia_core::{NullOutput, OpusEncoderOutput, PlaybackError, PlaybackSession};
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

    /// Stop playback without disconnecting its Discord voice transport.
    #[napi]
    pub async fn stop(&self, playback_id: String) -> napi::Result<()> {
        self.inner.playback(&playback_id)?.cancel_start();
        self.with_playback_operation(&playback_id, |session| session.stop())
            .await
            .map_err(|error| playback_error("could not stop playback", error))
    }

    /// Wait asynchronously until playback completes, stops, or fails.
    #[napi]
    pub async fn wait(&self, playback_id: String) -> napi::Result<String> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let session = Arc::clone(&self.inner.playback(&playback_id)?.session);
        let state = run_blocking(move || session.wait()).await?;
        Ok(playback_state_name(state).to_owned())
    }

    /// Current playback lifecycle state.
    #[napi]
    pub fn playback_state(&self, playback_id: String) -> napi::Result<String> {
        Ok(playback_state_name(self.inner.playback(&playback_id)?.session.state()).to_owned())
    }

    /// Playback submitted position and output-reported played position, in milliseconds.
    #[napi]
    pub fn playback_position(&self, playback_id: String) -> napi::Result<NativePlaybackPosition> {
        let position = self.inner.playback(&playback_id)?.session.position();
        Ok(NativePlaybackPosition {
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
