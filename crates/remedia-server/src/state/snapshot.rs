use super::*;

pub(crate) fn playback_snapshot(playback: &PlaybackSession, generation: u64) -> serde_json::Value {
    let position = playback.position();
    let media = position.media.map(|media| {
        serde_json::json!({
            "positionMs": media.position.as_millis(),
            "accuracy": match media.accuracy {
                remedia_core::MediaPositionAccuracy::Timestamp => "timestamp",
                remedia_core::MediaPositionAccuracy::Estimated => "estimated",
            },
        })
    });
    let processing_mode = playback.processing_mode().map(|mode| match mode {
        remedia_core::PlaybackProcessingMode::Passthrough => "passthrough",
        remedia_core::PlaybackProcessingMode::Transcoding => "transcoding",
    });
    let duration_ms = playback.duration().map(|duration| duration.as_millis());
    let last_error = playback.last_error().map(|_| {
        serde_json::json!({
            "code": "playback_failed",
            "message": "Playback failed; see server diagnostics for details.",
        })
    });
    let source = playback.info().map(|info| {
        serde_json::json!({
            "codec": info.codec,
            "sampleRate": info.sample_rate,
            "channels": info.channels,
            "channelLayout": info.channel_layout,
            "durationMs": info.duration.map(|duration| duration.as_millis()),
            "bitRate": info.bit_rate,
            "streamIndex": info.stream_index,
            "metadata": info.metadata,
        })
    });
    serde_json::json!({
        "state": playback_state_name(playback.state()),
        "generation": generation,
        "processingMode": processing_mode,
        "position": {
            "media": media,
            "submittedMs": position.submitted.as_millis(),
            "playedMs": position.played.map(|duration| duration.as_millis()),
        },
        "durationMs": duration_ms,
        "source": source,
        "filterGraph": playback.filter_graph(),
        "volume": playback.volume(),
        "lastError": last_error,
    })
}

pub(crate) fn playback_state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Idle => "idle",
        PlaybackState::Starting => "starting",
        PlaybackState::Playing => "playing",
        PlaybackState::Pausing => "pausing",
        PlaybackState::Resuming => "resuming",
        PlaybackState::Paused => "paused",
        PlaybackState::Stopping => "stopping",
        PlaybackState::Stopped => "stopped",
        PlaybackState::Completed => "completed",
        PlaybackState::Failed => "failed",
    }
}
