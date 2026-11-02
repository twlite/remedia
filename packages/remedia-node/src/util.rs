use std::time::Duration;

use napi::bindgen_prelude::Status;
use remedia_core::{AudioStreamInfo, InputSource, PlaybackError, PlaybackState};
use remedia_discord::{ConnectionState, VoiceError};
use remedia_observability::ObservabilityError;

use crate::NativeResult;
use crate::types::{NativeMediaSource, NativeStreamInfo};

pub(crate) fn parse_source(source: NativeMediaSource) -> NativeResult<InputSource> {
    match source.kind.as_str() {
        "file" => {
            let value = source_value(source.value, "file path")?;
            Ok(InputSource::file(value))
        }
        "url" => {
            let value = source_value(source.value, "media URL")?;
            Ok(InputSource::http_url(value))
        }
        "buffer" => {
            let bytes = source.bytes.ok_or_else(|| {
                napi_error(
                    Status::InvalidArg,
                    "buffer media source requires a byte buffer",
                )
            })?;
            Ok(InputSource::bytes(Vec::<u8>::from(bytes)))
        }
        "stream" => Err(napi_error(
            Status::InvalidArg,
            "stream media sources must be created by the Remedia byte-stream API",
        )),
        _ => Err(napi_error(
            Status::InvalidArg,
            "media source kind must be `file`, `url`, `buffer`, or `stream`",
        )),
    }
}

fn source_value(value: Option<String>, description: &str) -> NativeResult<String> {
    let value = value.ok_or_else(|| {
        napi_error(
            Status::InvalidArg,
            format!("media source requires a {description}"),
        )
    })?;
    if value.is_empty() {
        return Err(napi_error(
            Status::InvalidArg,
            format!("media {description} cannot be empty"),
        ));
    }
    Ok(value)
}

pub(crate) fn parse_snowflake(field: &'static str, value: &str) -> NativeResult<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(napi_error(
            Status::InvalidArg,
            format!("{field} must be a non-zero decimal Discord snowflake string"),
        ));
    }
    let id = value.parse::<u64>().map_err(|_| {
        napi_error(
            Status::InvalidArg,
            format!("{field} is outside the supported Discord snowflake range"),
        )
    })?;
    if id == 0 {
        return Err(napi_error(
            Status::InvalidArg,
            format!("{field} must be a non-zero Discord snowflake string"),
        ));
    }
    Ok(id)
}

pub(crate) fn duration_from_millis(value: f64) -> NativeResult<Duration> {
    if !value.is_finite() || value < 0.0 {
        return Err(napi_error(
            Status::InvalidArg,
            "position must be a finite non-negative number of milliseconds",
        ));
    }
    Duration::try_from_secs_f64(value / 1000.0).map_err(|_| {
        napi_error(
            Status::InvalidArg,
            "position is outside the supported duration range",
        )
    })
}

pub(crate) fn duration_millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

pub(crate) fn native_stream_info(info: AudioStreamInfo) -> NativeStreamInfo {
    NativeStreamInfo {
        stream_index: u32::try_from(info.stream_index).unwrap_or(u32::MAX),
        codec: info.codec,
        sample_rate: info.sample_rate,
        channels: info.channels,
        channel_layout: info.channel_layout,
        duration_ms: info.duration.map(duration_millis),
        bit_rate: info.bit_rate.map(|bit_rate| bit_rate as f64),
        metadata: info.metadata.into_iter().collect(),
    }
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

pub(crate) fn connection_state_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::NegotiatingDave => "negotiatingDave",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Disconnecting => "disconnecting",
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Failed => "failed",
    }
}

pub(crate) fn is_active(state: PlaybackState) -> bool {
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

pub(crate) fn playback_error(context: &str, error: PlaybackError) -> napi::Error {
    napi_error(Status::GenericFailure, format!("{context}: {error}"))
}

pub(crate) fn voice_error(context: &str, error: VoiceError) -> napi::Error {
    napi_error(Status::GenericFailure, format!("{context}: {error}"))
}

pub(crate) fn observability_error(context: &str, error: ObservabilityError) -> napi::Error {
    napi_error(Status::GenericFailure, format!("{context}: {error}"))
}

pub(crate) fn napi_error(status: Status, reason: impl Into<String>) -> napi::Error {
    napi::Error::new(status, reason.into())
}
