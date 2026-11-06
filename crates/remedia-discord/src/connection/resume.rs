use super::*;

pub(super) async fn resume_gateway(
    info: &VoiceConnectionInfo,
    sequence_ack: Option<u16>,
) -> Result<VoiceWebSocket, VoiceError> {
    let endpoint = voice_endpoint(&info.endpoint)?;
    let (mut websocket, _) = time::timeout(CONNECT_TIMEOUT, connect_async(endpoint.as_str()))
        .await
        .map_err(|_| VoiceError::Timeout("reconnecting to the voice gateway"))??;
    send_json(
        &mut websocket,
        &json_message(
            7,
            json!({
                "server_id": info.guild_id.to_string(),
                "session_id": info.session_id,
                "token": info.token,
                "seq_ack": sequence_ack.map_or(-1_i64, i64::from),
            }),
        ),
    )
    .await?;
    loop {
        let message = time::timeout(HANDSHAKE_TIMEOUT, websocket.next())
            .await
            .map_err(|_| VoiceError::Timeout("waiting for voice Resume"))?
            .ok_or_else(|| {
                VoiceError::ResumeFailed("gateway closed the resumed socket".into())
            })??;
        match message {
            Message::Ping(payload) => {
                send_ws_message(&mut websocket, Message::Pong(payload)).await?
            }
            Message::Close(_) => {
                return Err(VoiceError::ResumeFailed(
                    "gateway closed the resumed socket".into(),
                ));
            }
            message => {
                let Some(message) = GatewayMessage::from_ws(message)? else {
                    continue;
                };
                if let GatewayMessage::Json { opcode: 9, .. } = message {
                    return Ok(websocket);
                }
            }
        }
    }
}

pub(super) async fn resume_gateway_cancellable(
    info: &VoiceConnectionInfo,
    sequence_ack: Option<u16>,
    cancellation: &TokioCancellationToken,
) -> Result<VoiceWebSocket, VoiceError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(VoiceError::Cancelled),
        result = resume_gateway(info, sequence_ack) => result,
    }
}

pub(super) async fn resume_for_actor(
    info: &VoiceConnectionInfo,
    sequence_ack: Option<u16>,
    cancellation: &TokioCancellationToken,
) -> Result<Option<VoiceWebSocket>, VoiceError> {
    match resume_gateway_cancellable(info, sequence_ack, cancellation).await {
        Err(VoiceError::Cancelled) if cancellation.is_cancelled() => Ok(None),
        Ok(websocket) => Ok(Some(websocket)),
        Err(error) => Err(error),
    }
}
