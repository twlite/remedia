//! RPC validation, error mapping, and response formatting.

use super::*;

pub(crate) fn require_client(
    connection: &ConnectionContext,
) -> Result<Arc<ClientSession>, RpcError> {
    let session = connection.session().ok_or_else(|| {
        RpcError::new("unknown_session", "create or resume a client session first")
    })?;
    if session.principal != connection.principal || !session.attached_to(&connection.id) {
        return Err(RpcError::new(
            "unknown_session",
            "client session is not owned by this connection",
        ));
    }
    session.ensure_open().map_err(state_error)?;
    Ok(session)
}

pub(crate) fn decode_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|_| {
        RpcError::new(
            "invalid_request",
            "command params have invalid fields or types",
        )
    })
}

pub(super) fn validate_snowflake(value: &str, field: &'static str) -> Result<u64, RpcError> {
    parse_snowflake(value)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            RpcError::new(
                "invalid_request",
                format!("{field} must be a positive decimal u64 string"),
            )
        })
}

pub(super) fn parse_snowflake(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

pub(super) fn valid_discord_voice_endpoint(endpoint: &str) -> bool {
    let endpoint = if endpoint.starts_with("wss://") || endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("wss://{endpoint}")
    };
    let Ok(endpoint) = url::Url::parse(&endpoint) else {
        return false;
    };
    let Some(host) = endpoint.host_str() else {
        return false;
    };
    endpoint.scheme() == "wss"
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.query().is_none()
        && endpoint.fragment().is_none()
        && (host.eq_ignore_ascii_case("discord.media")
            || host.to_ascii_lowercase().ends_with(".discord.media"))
}

pub(super) fn ensure_param_guild(
    param_guild_id: &str,
    selected_guild_id: &str,
) -> Result<(), RpcError> {
    if param_guild_id == selected_guild_id {
        Ok(())
    } else {
        Err(RpcError::new(
            "invalid_request",
            "guildId changed while processing the request",
        ))
    }
}

pub(super) fn state_error(error: StateError) -> RpcError {
    match error {
        StateError::UnknownSession => {
            RpcError::new("unknown_session", "session or player was not found")
        }
        StateError::InvalidState => {
            RpcError::new("invalid_state", "operation is invalid in the current state")
        }
        StateError::ResourceExhausted => RpcError::new(
            "resource_exhausted",
            "configured server capacity is exhausted",
        ),
        StateError::Cleanup => RpcError::new(
            "transport_failed",
            "session cleanup encountered a playback or transport failure",
        ),
    }
}

pub(crate) fn media_error(_error: MediaAccessError) -> RpcError {
    RpcError::new(
        "invalid_request",
        "media source is not allowed by server policy",
    )
}

pub(super) fn spawn_observed<T>(
    future: impl Future<Output = T> + Send + 'static,
    observability: &ObservabilityHandle,
) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
{
    if let Some(dispatch) = observability.dispatch().cloned() {
        tokio::spawn(future.with_subscriber(dispatch))
    } else {
        tokio::spawn(future.with_subscriber(tracing::Dispatch::none()))
    }
}

pub(super) fn playback_error(error: PlaybackError) -> RpcError {
    let code = playback_error_code(&error);
    let message = match code {
        "invalid_state" => "playback operation is invalid in the current state",
        "unsupported" => "playback operation is unsupported for this source",
        _ => "playback operation failed; inspect server diagnostics for details",
    };
    RpcError::new(code, message)
}

pub(super) fn playback_error_code(error: &PlaybackError) -> &'static str {
    match error {
        PlaybackError::InvalidState { .. } => "invalid_state",
        PlaybackError::UnsupportedSeeking(_) | PlaybackError::SeekOutOfRange { .. } => {
            "unsupported"
        }
        PlaybackError::Cancelled => "invalid_state",
        _ => "playback_failed",
    }
}

pub(crate) fn stream_info_json(info: remedia_core::AudioStreamInfo) -> Value {
    json!({
        "streamIndex": info.stream_index,
        "codec": info.codec,
        "sampleRate": info.sample_rate,
        "channels": info.channels,
        "channelLayout": info.channel_layout,
        "durationMs": info.duration.map(|duration| duration.as_millis()),
        "bitRate": info.bit_rate,
        "metadata": info.metadata,
    })
}

/// Claim a monotonically increasing transport-operation generation from a client.
/// WebSocket handlers execute concurrently, so the server must not rely on task
/// scheduling to preserve request arrival order. Older clients omit the field;
/// those requests receive a server-generated generation for compatibility.
pub(super) fn claim_voice_generation(
    generation: &AtomicU64,
    requested: Option<u64>,
) -> Result<u64, RpcError> {
    let Some(requested) = requested else {
        return Ok(generation.fetch_add(1, Ordering::AcqRel) + 1);
    };
    if requested == 0 {
        return Err(RpcError::new(
            "invalid_request",
            "transportGeneration must be a positive integer",
        ));
    }

    let mut current = generation.load(Ordering::Acquire);
    loop {
        if requested < current {
            return Err(RpcError::new(
                "invalid_state",
                "Discord voice operation was superseded by a newer transport generation",
            ));
        }
        if requested == current {
            return Ok(requested);
        }
        match generation.compare_exchange_weak(
            current,
            requested,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(requested),
            Err(actual) => current = actual,
        }
    }
}

pub(super) fn playback_start_json(info: remedia_core::AudioStreamInfo, generation: u64) -> Value {
    let mut result = stream_info_json(info);
    result["generation"] = json!(generation);
    result
}

pub(super) fn voice_state_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::NegotiatingDave => "negotiating_dave",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Disconnecting => "disconnecting",
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Failed => "failed",
    }
}
