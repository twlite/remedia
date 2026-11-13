use std::collections::{HashMap, HashSet};
use std::num::NonZeroU16;

use davey::{DaveSession, ProposalsOperationType};
use serde_json::{Value, json};

use crate::error::VoiceError;
use crate::protocol::{GatewayMessage, binary_client_message, json_message, parse_u16, parse_u64};

pub(crate) enum OutgoingGatewayMessage {
    Json(Value),
    Binary(Vec<u8>),
}

/// Owns the DAVE session and the gateway transition state for one connection.
/// MLS, media ratchets, and encoded-frame encryption remain inside `davey`.
pub(crate) struct DaveProtocol {
    session: DaveSession,
    protocol_version: u16,
    user_id: u64,
    channel_id: u64,
    recognized_user_ids: HashSet<u64>,
    pending_transitions: HashMap<u16, u16>,
}

impl DaveProtocol {
    pub(crate) fn new(
        protocol_version: u16,
        user_id: u64,
        channel_id: u64,
    ) -> Result<Self, VoiceError> {
        if protocol_version == 0 {
            return Err(VoiceError::DaveRequired);
        }
        let session = create_session(protocol_version, user_id, channel_id)?;
        Ok(Self {
            session,
            protocol_version,
            user_id,
            channel_id,
            recognized_user_ids: HashSet::from([user_id]),
            pending_transitions: HashMap::new(),
        })
    }

    pub(crate) fn create_key_package(&mut self) -> Result<Vec<u8>, VoiceError> {
        self.session
            .create_key_package()
            .map(|package| binary_client_message(26, &package))
            .map_err(|error| VoiceError::Dave(error.to_string()))
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.session.can_encrypt()
    }

    pub(crate) fn epoch(&self) -> Option<u64> {
        self.session.epoch().map(|epoch| epoch.as_u64())
    }

    pub(crate) fn member_count(&self) -> usize {
        self.recognized_user_ids.len()
    }

    pub(crate) fn encrypt_opus<'a>(
        &mut self,
        opus: &'a [u8],
    ) -> Result<std::borrow::Cow<'a, [u8]>, VoiceError> {
        if !self.is_ready() {
            return Err(VoiceError::DaveNotReady);
        }
        self.session
            .encrypt_opus(opus)
            .map_err(|error| VoiceError::Dave(error.to_string()))
    }

    pub(crate) fn handle(
        &mut self,
        message: &GatewayMessage,
    ) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        match message {
            GatewayMessage::Json {
                opcode: 11, data, ..
            } => {
                let users = data
                    .get("user_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        VoiceError::Protocol("DAVE client-connect event omitted `user_ids`".into())
                    })?;
                for user in users {
                    self.recognized_user_ids
                        .insert(parse_u64(user, "user_ids")?);
                }
                Ok(Vec::new())
            }
            GatewayMessage::Json {
                opcode: 13, data, ..
            } => {
                let user = data.get("user_id").ok_or_else(|| {
                    VoiceError::Protocol("DAVE client-disconnect event omitted `user_id`".into())
                })?;
                self.recognized_user_ids
                    .remove(&parse_u64(user, "user_id")?);
                Ok(Vec::new())
            }
            GatewayMessage::Json {
                opcode: 21, data, ..
            } => self.prepare_transition(data),
            GatewayMessage::Json {
                opcode: 22, data, ..
            } => self.execute_transition(data),
            GatewayMessage::Json {
                opcode: 24, data, ..
            } => self.prepare_epoch(data),
            GatewayMessage::Json {
                opcode: 31, data, ..
            } => {
                let transition_id = data
                    .get("transition_id")
                    .map(|value| parse_u16(value, "transition_id"))
                    .transpose()?
                    .unwrap_or(0);
                self.recover(transition_id, false)
            }
            GatewayMessage::Binary {
                opcode: 25, data, ..
            } => {
                self.session
                    .set_external_sender(data)
                    .map_err(|error| VoiceError::Dave(error.to_string()))?;
                Ok(Vec::new())
            }
            GatewayMessage::Binary {
                opcode: 27, data, ..
            } => self.process_proposals(data),
            GatewayMessage::Binary {
                opcode: 29, data, ..
            } => self.process_commit(data),
            GatewayMessage::Binary {
                opcode: 30, data, ..
            } => self.process_welcome(data),
            _ => Ok(Vec::new()),
        }
    }

    fn prepare_transition(
        &mut self,
        data: &Value,
    ) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        let transition_id = data
            .get("transition_id")
            .ok_or_else(|| {
                VoiceError::Protocol("DAVE prepare-transition omitted `transition_id`".into())
            })
            .and_then(|value| parse_u16(value, "transition_id"))?;
        let protocol_version = data
            .get("protocol_version")
            .ok_or_else(|| {
                VoiceError::Protocol("DAVE prepare-transition omitted `protocol_version`".into())
            })
            .and_then(|value| parse_u16(value, "protocol_version"))?;
        if protocol_version == 0 {
            // Do not honor a downgrade that would make the media path plaintext.
            return Err(VoiceError::DaveRequired);
        }
        validate_protocol_version(protocol_version)?;

        self.session
            .prepare_transition(transition_id, protocol_version)
            .map_err(|error| VoiceError::Dave(error.to_string()))?;
        if transition_id == 0 {
            self.protocol_version = protocol_version;
            return Ok(Vec::new());
        }

        self.pending_transitions
            .insert(transition_id, protocol_version);
        Ok(vec![OutgoingGatewayMessage::Json(json_message(
            23,
            json!({
                "transition_id": transition_id,
                "protocol_version": protocol_version,
            }),
        ))])
    }

    fn execute_transition(
        &mut self,
        data: &Value,
    ) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        let transition_id = data
            .get("transition_id")
            .ok_or_else(|| {
                VoiceError::Protocol("DAVE execute-transition omitted `transition_id`".into())
            })
            .and_then(|value| parse_u16(value, "transition_id"))?;
        if transition_id == 0 {
            return Ok(Vec::new());
        }
        let protocol_version =
            self.pending_transitions
                .remove(&transition_id)
                .ok_or_else(|| {
                    VoiceError::Protocol(format!(
                        "DAVE executed unknown transition {transition_id}"
                    ))
                })?;
        if !self
            .session
            .execute_transition(transition_id)
            .map_err(|error| VoiceError::Dave(error.to_string()))?
        {
            return Err(VoiceError::Protocol(format!(
                "DAVE transition {transition_id} was not prepared"
            )));
        }
        self.protocol_version = protocol_version;
        Ok(Vec::new())
    }

    fn prepare_epoch(&mut self, data: &Value) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        let epoch = data
            .get("epoch")
            .ok_or_else(|| VoiceError::Protocol("DAVE prepare-epoch omitted `epoch`".into()))
            .and_then(|value| parse_u64(value, "epoch"))?;
        let version = data
            .get("protocol_version")
            .ok_or_else(|| {
                VoiceError::Protocol("DAVE prepare-epoch omitted `protocol_version`".into())
            })
            .and_then(|value| parse_u16(value, "protocol_version"))?;
        if version == 0 {
            return Err(VoiceError::DaveRequired);
        }
        validate_protocol_version(version)?;

        if epoch == 1 {
            self.reinitialize(version)?;
            return Ok(vec![OutgoingGatewayMessage::Binary(
                self.create_key_package()?,
            )]);
        }
        let current_epoch = self
            .session
            .epoch()
            .map(|epoch| epoch.as_u64())
            .ok_or_else(|| {
                VoiceError::Protocol(
                    "DAVE prepare-epoch arrived before an MLS group existed".into(),
                )
            })?;
        if epoch > current_epoch {
            // A new epoch is established by the following MLS commit or
            // welcome. Readiness is reported only after that message is
            // processed, in process_commit/process_welcome.
            return Ok(Vec::new());
        }
        if epoch < current_epoch {
            return Err(VoiceError::Protocol(format!(
                "DAVE prepared epoch {epoch} behind current epoch {current_epoch}"
            )));
        }
        if version != self.protocol_version {
            return Err(VoiceError::UnsupportedDaveVersion(version));
        }
        // Opcode 24 has no transition ID. Keep the current group and media
        // keys; opcode 21 supplies the ID for a same-epoch transition.
        Ok(Vec::new())
    }

    fn process_proposals(
        &mut self,
        data: &[u8],
    ) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        let Some((&operation, proposals)) = data.split_first() else {
            return Err(VoiceError::Protocol(
                "DAVE proposals binary payload was empty".into(),
            ));
        };
        let operation = match operation {
            0 => ProposalsOperationType::APPEND,
            1 => ProposalsOperationType::REVOKE,
            _ => {
                return Err(VoiceError::Protocol(
                    "DAVE proposal operation was unknown".into(),
                ));
            }
        };
        let expected_users: Vec<_> = self.recognized_user_ids.iter().copied().collect();
        let result = self
            .session
            .process_proposals(operation, proposals, Some(&expected_users))
            .map_err(|error| VoiceError::Dave(error.to_string()))?;
        let Some(result) = result else {
            return Ok(Vec::new());
        };

        let mut message = Vec::with_capacity(
            1 + result.commit.len() + result.welcome.as_ref().map_or(0, Vec::len),
        );
        message.push(28);
        message.extend_from_slice(&result.commit);
        if let Some(welcome) = result.welcome {
            message.extend_from_slice(&welcome);
        }
        Ok(vec![OutgoingGatewayMessage::Binary(message)])
    }

    fn process_commit(&mut self, data: &[u8]) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        let (transition_id, commit) = split_transition_id(data)?;
        match self.session.process_commit(commit) {
            Ok(false) => Ok(Vec::new()),
            Ok(true) => self.prepare_group_transition(transition_id),
            Err(_) => self.recover(transition_id, true),
        }
    }

    fn process_welcome(&mut self, data: &[u8]) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        let (transition_id, welcome) = split_transition_id(data)?;
        match self.session.process_welcome(
            welcome,
            Some(&self.recognized_user_ids.iter().copied().collect::<Vec<_>>()),
        ) {
            Ok(()) => self.prepare_group_transition(transition_id),
            Err(_) => self.recover(transition_id, true),
        }
    }

    fn prepare_group_transition(
        &mut self,
        transition_id: u16,
    ) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        self.session
            .prepare_transition(transition_id, self.protocol_version)
            .map_err(|error| VoiceError::Dave(error.to_string()))?;
        if transition_id == 0 {
            return Ok(Vec::new());
        }
        self.pending_transitions
            .insert(transition_id, self.protocol_version);
        Ok(vec![OutgoingGatewayMessage::Json(json_message(
            23,
            json!({
                "transition_id": transition_id,
                "protocol_version": self.protocol_version,
            }),
        ))])
    }

    fn recover(
        &mut self,
        transition_id: u16,
        notify_gateway: bool,
    ) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
        self.reinitialize(self.protocol_version)?;
        let key_package = self.create_key_package()?;
        let mut outgoing = Vec::with_capacity(2);
        if notify_gateway {
            outgoing.push(OutgoingGatewayMessage::Json(json_message(
                31,
                json!({ "transition_id": transition_id }),
            )));
        }
        outgoing.push(OutgoingGatewayMessage::Binary(key_package));
        Ok(outgoing)
    }

    fn reinitialize(&mut self, protocol_version: u16) -> Result<(), VoiceError> {
        let version = NonZeroU16::new(protocol_version).ok_or(VoiceError::DaveRequired)?;
        self.session
            .reinit(version, self.user_id, self.channel_id, None)
            .map_err(|error| VoiceError::Dave(error.to_string()))?;
        self.protocol_version = protocol_version;
        self.pending_transitions.clear();
        Ok(())
    }
}

fn create_session(
    protocol_version: u16,
    user_id: u64,
    channel_id: u64,
) -> Result<DaveSession, VoiceError> {
    validate_protocol_version(protocol_version)?;
    let version = NonZeroU16::new(protocol_version).ok_or(VoiceError::DaveRequired)?;
    DaveSession::new(version, user_id, channel_id, None)
        .map_err(|error| VoiceError::Dave(error.to_string()))
}

fn validate_protocol_version(protocol_version: u16) -> Result<(), VoiceError> {
    if protocol_version != davey::DAVE_PROTOCOL_VERSION {
        return Err(VoiceError::UnsupportedDaveVersion(protocol_version));
    }
    Ok(())
}

fn split_transition_id(data: &[u8]) -> Result<(u16, &[u8]), VoiceError> {
    if data.len() < 3 {
        return Err(VoiceError::Protocol(
            "DAVE transition message was missing its ID or MLS message".into(),
        ));
    }
    Ok((u16::from_be_bytes([data[0], data[1]]), &data[2..]))
}

#[cfg(test)]
mod tests;
