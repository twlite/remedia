use std::net::IpAddr;

use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::error::VoiceError;

#[derive(Clone, Debug)]
pub(crate) enum GatewayMessage {
    Json {
        opcode: u64,
        data: Value,
        sequence: Option<u16>,
    },
    Binary {
        opcode: u8,
        data: Vec<u8>,
        sequence: u16,
    },
}

impl GatewayMessage {
    pub(crate) fn from_ws(message: Message) -> Result<Option<Self>, VoiceError> {
        match message {
            Message::Text(text) => {
                let payload: Value = serde_json::from_str(text.as_str())?;
                let opcode = payload.get("op").and_then(Value::as_u64).ok_or_else(|| {
                    VoiceError::Protocol("JSON message has no numeric `op`".into())
                })?;
                let data = payload.get("d").cloned().unwrap_or(Value::Null);
                let sequence = match payload.get("seq") {
                    Some(value) => Some(
                        value
                            .as_u64()
                            .and_then(|number| u16::try_from(number).ok())
                            .ok_or_else(|| {
                                VoiceError::Protocol("JSON sequence number is out of range".into())
                            })?,
                    ),
                    None => None,
                };
                Ok(Some(Self::Json {
                    opcode,
                    data,
                    sequence,
                }))
            }
            Message::Binary(bytes) => {
                let bytes = bytes.as_ref();
                if bytes.len() < 3 {
                    return Err(VoiceError::Protocol(
                        "binary gateway message was shorter than its sequence and opcode header"
                            .into(),
                    ));
                }
                Ok(Some(Self::Binary {
                    sequence: u16::from_be_bytes([bytes[0], bytes[1]]),
                    opcode: bytes[2],
                    data: bytes[3..].to_vec(),
                }))
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
            Message::Close(_) => Err(VoiceError::Protocol(
                "voice WebSocket closed by the gateway".into(),
            )),
        }
    }

    pub(crate) fn sequence(&self) -> Option<u16> {
        match self {
            Self::Json { sequence, .. } => *sequence,
            Self::Binary { sequence, .. } => Some(*sequence),
        }
    }
}

pub(crate) fn json_message(opcode: u64, data: Value) -> Value {
    json!({ "op": opcode, "d": data })
}

pub(crate) fn binary_client_message(opcode: u8, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() + 1);
    bytes.push(opcode);
    bytes.extend_from_slice(data);
    bytes
}

pub(crate) fn parse_u64(value: &Value, field: &'static str) -> Result<u64, VoiceError> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .ok_or_else(|| VoiceError::Protocol(format!("voice gateway field `{field}` was invalid")))
}

pub(crate) fn parse_u16(value: &Value, field: &'static str) -> Result<u16, VoiceError> {
    let value = parse_u64(value, field)?;
    u16::try_from(value)
        .map_err(|_| VoiceError::Protocol(format!("voice gateway field `{field}` exceeded u16")))
}

pub(crate) fn parse_ip_discovery_response(
    bytes: &[u8],
    expected_ssrc: u32,
) -> Result<(IpAddr, u16), VoiceError> {
    if bytes.len() < 74 {
        return Err(VoiceError::Protocol(
            "UDP discovery response was shorter than 74 bytes".into(),
        ));
    }
    if u16::from_be_bytes([bytes[0], bytes[1]]) != 2
        || u16::from_be_bytes([bytes[2], bytes[3]]) != 70
    {
        return Err(VoiceError::Protocol(
            "UDP discovery response had an invalid type or length".into(),
        ));
    }
    let ssrc = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if ssrc != expected_ssrc {
        return Err(VoiceError::Protocol(
            "UDP discovery response had a mismatched SSRC".into(),
        ));
    }
    let address_bytes = &bytes[8..72];
    let address_end = address_bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| {
            VoiceError::Protocol("UDP discovery address was not null terminated".into())
        })?;
    let address = std::str::from_utf8(&address_bytes[..address_end])
        .map_err(|_| VoiceError::Protocol("UDP discovery address was not UTF-8".into()))?
        .parse::<IpAddr>()
        .map_err(|_| VoiceError::Protocol("UDP discovery address was not an IP address".into()))?;
    let port = u16::from_be_bytes([bytes[72], bytes[73]]);
    if port == 0 {
        return Err(VoiceError::Protocol("UDP discovery returned port 0".into()));
    }
    Ok((address, port))
}

pub(crate) fn ip_discovery_request(ssrc: u32) -> [u8; 74] {
    let mut packet = [0; 74];
    packet[..2].copy_from_slice(&1_u16.to_be_bytes());
    packet[2..4].copy_from_slice(&70_u16.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gateway_json_and_sequence_numbers() {
        let message = GatewayMessage::from_ws(Message::Text(
            r#"{"op":8,"d":{"heartbeat_interval":45000},"seq":65535}"#.into(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(message.sequence(), Some(u16::MAX));
        assert!(matches!(message, GatewayMessage::Json { opcode: 8, .. }));
    }

    #[test]
    fn parses_voice_binary_sequence_opcode_and_payload() {
        let message = GatewayMessage::from_ws(Message::Binary(vec![0x12, 0x34, 25, 1, 2].into()))
            .unwrap()
            .unwrap();
        match message {
            GatewayMessage::Binary {
                opcode,
                data,
                sequence,
            } => {
                assert_eq!(sequence, 0x1234);
                assert_eq!(opcode, 25);
                assert_eq!(data, [1, 2]);
            }
            _ => panic!("expected a binary gateway message"),
        }
    }

    #[test]
    fn validates_ip_discovery_response() {
        let mut response = [0; 74];
        response[..2].copy_from_slice(&2_u16.to_be_bytes());
        response[2..4].copy_from_slice(&70_u16.to_be_bytes());
        response[4..8].copy_from_slice(&123_u32.to_be_bytes());
        response[8..17].copy_from_slice(b"192.0.2.1");
        response[72..74].copy_from_slice(&5000_u16.to_be_bytes());
        assert_eq!(
            parse_ip_discovery_response(&response, 123).unwrap(),
            ("192.0.2.1".parse().unwrap(), 5000)
        );
        assert!(parse_ip_discovery_response(&response, 124).is_err());
    }
}
