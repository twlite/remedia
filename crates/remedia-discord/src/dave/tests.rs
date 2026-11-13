use super::*;

use openmls::prelude::{tls_codec::Serialize, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

#[test]
fn dave_session_starts_fail_closed_until_an_mls_group_is_ready() {
    let mut session = DaveProtocol::new(davey::DAVE_PROTOCOL_VERSION, 100, 200).unwrap();
    assert!(!session.is_ready());
    assert!(matches!(
        session.encrypt_opus(&[1, 2, 3]),
        Err(VoiceError::DaveNotReady)
    ));
}

#[test]
fn unsupported_dave_protocol_versions_are_rejected() {
    assert!(matches!(
        DaveProtocol::new(davey::DAVE_PROTOCOL_VERSION + 1, 100, 200),
        Err(VoiceError::UnsupportedDaveVersion(_))
    ));
    assert!(matches!(
        DaveProtocol::new(0, 100, 200),
        Err(VoiceError::DaveRequired)
    ));
}

#[test]
fn prepare_transition_requires_nonzero_dave_and_returns_ready_message() {
    let mut session = DaveProtocol::new(davey::DAVE_PROTOCOL_VERSION, 100, 200).unwrap();
    let response = session
        .handle(&GatewayMessage::Json {
            opcode: 21,
            data: json!({ "transition_id": 17, "protocol_version": 1 }),
            sequence: None,
        })
        .unwrap();
    assert!(
        matches!(response.as_slice(), [OutgoingGatewayMessage::Json(value)] if value["op"] == 23 && value["d"]["transition_id"] == 17)
    );

    let downgrade = session.handle(&GatewayMessage::Json {
        opcode: 21,
        data: json!({ "transition_id": 18, "protocol_version": 0 }),
        sequence: None,
    });
    assert!(matches!(downgrade, Err(VoiceError::DaveRequired)));
}

#[test]
fn sole_member_reset_reinitializes_mls_without_epoch_transition_id() {
    let mut session = DaveProtocol::new(davey::DAVE_PROTOCOL_VERSION, 100, 200).unwrap();
    let initial = session
        .handle(&GatewayMessage::Json {
            opcode: 24,
            data: json!({
                "epoch": 1,
                "protocol_version": davey::DAVE_PROTOCOL_VERSION,
            }),
            sequence: None,
        })
        .unwrap();
    assert!(
        matches!(initial.as_slice(), [OutgoingGatewayMessage::Binary(bytes)] if bytes.first() == Some(&26))
    );

    let transition = session
        .handle(&GatewayMessage::Json {
            opcode: 21,
            data: json!({
                "transition_id": 0,
                "protocol_version": davey::DAVE_PROTOCOL_VERSION,
            }),
            sequence: None,
        })
        .unwrap();
    assert!(transition.is_empty());

    let later = session.handle(&GatewayMessage::Json {
        opcode: 24,
        data: json!({
            "epoch": 2,
            "protocol_version": davey::DAVE_PROTOCOL_VERSION,
        }),
        sequence: None,
    });
    assert!(matches!(later, Err(VoiceError::Protocol(_))));
}

/// A local voice gateway producing signed, spec-compliant external add proposals.
struct TestGateway {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    sender: ExternalSender,
}

impl TestGateway {
    fn new() -> Self {
        let signer = SignatureKeyPair::new(
            Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256.signature_algorithm(),
        )
        .unwrap();
        let sender = ExternalSender::new(
            signer.public().into(),
            BasicCredential::new(vec![1, 2, 3]).into(),
        );

        Self {
            provider: OpenMlsRustCrypto::default(),
            signer,
            sender,
        }
    }

    fn member(&self, user_id: u64) -> DaveProtocol {
        let mut member = DaveProtocol::new(1, user_id, 200).unwrap();
        member
            .handle(&GatewayMessage::Binary {
                opcode: 25,
                data: self.sender.tls_serialize_detached().unwrap(),
                sequence: 1,
            })
            .unwrap();
        member
    }

    fn add(&self, member: &mut DaveProtocol, joiner: &mut DaveProtocol) -> davey::CommitWelcome {
        let package = joiner.session.create_key_package().unwrap();
        let package = KeyPackageIn::tls_deserialize_exact_bytes(&package)
            .unwrap()
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            package,
            GroupId::from_slice(&200_u64.to_be_bytes()),
            GroupEpoch::from(member.epoch().unwrap()),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        let proposals = VLBytes::new(proposal.tls_serialize_detached().unwrap())
            .tls_serialize_detached()
            .unwrap();
        member.recognized_user_ids.insert(joiner.user_id);
        joiner.recognized_user_ids = member.recognized_user_ids.clone();

        member
            .session
            .process_proposals(
                ProposalsOperationType::APPEND,
                &proposals,
                Some(
                    &member
                        .recognized_user_ids
                        .iter()
                        .copied()
                        .collect::<Vec<_>>(),
                ),
            )
            .unwrap()
            .unwrap()
    }
}

fn handle_json(
    member: &mut DaveProtocol,
    opcode: u64,
    data: Value,
) -> Result<Vec<OutgoingGatewayMessage>, VoiceError> {
    member.handle(&GatewayMessage::Json {
        opcode,
        data,
        sequence: None,
    })
}

fn handle_mls(
    member: &mut DaveProtocol,
    opcode: u8,
    id: u16,
    payload: &[u8],
) -> Vec<OutgoingGatewayMessage> {
    let mut data = id.to_be_bytes().to_vec();
    data.extend_from_slice(payload);
    member
        .handle(&GatewayMessage::Binary {
            opcode,
            data,
            sequence: 2,
        })
        .unwrap()
}

fn assert_ready_ack(messages: &[OutgoingGatewayMessage], id: u16) {
    assert!(matches!(messages, [OutgoingGatewayMessage::Json(value)]
        if value["op"] == 23 && value["d"]["transition_id"] == id));
}

fn execute(member: &mut DaveProtocol, id: u16) {
    assert!(
        handle_json(member, 22, json!({ "transition_id": id }))
            .unwrap()
            .is_empty()
    );
}

fn assert_encrypted_roundtrip(sender: &mut DaveProtocol, receiver: &mut DaveProtocol) {
    let opus = [1, 2, 3];
    let encrypted = sender.encrypt_opus(&opus).unwrap();
    assert_ne!(encrypted.as_ref(), &opus);
    assert_eq!(
        receiver
            .session
            .decrypt(sender.user_id, davey::MediaType::AUDIO, &encrypted)
            .unwrap(),
        opus
    );
}

/// Epoch 2 avoids conflating same-epoch announcements with the special epoch 1 reset.
fn established_group() -> (TestGateway, DaveProtocol, DaveProtocol) {
    let gateway = TestGateway::new();
    let mut a = gateway.member(100);
    let mut b = gateway.member(101);
    let first = gateway.add(&mut a, &mut b);
    assert_ready_ack(&handle_mls(&mut a, 29, 10, &first.commit), 10);
    assert_ready_ack(&handle_mls(&mut b, 30, 10, &first.welcome.unwrap()), 10);
    execute(&mut a, 10);
    execute(&mut b, 10);

    let mut c = gateway.member(102);
    let second = gateway.add(&mut a, &mut c);
    assert_ready_ack(&handle_mls(&mut a, 29, 11, &second.commit), 11);
    assert_ready_ack(&handle_mls(&mut c, 30, 11, &second.welcome.unwrap()), 11);
    execute(&mut a, 11);
    execute(&mut c, 11);
    assert_eq!(a.epoch(), Some(2));
    assert!(a.is_ready());
    assert!(c.is_ready());

    (gateway, a, c)
}

#[test]
fn same_epoch_announcement_waits_for_opcode_21_transition_id() {
    let (_, mut member, mut receiver) = established_group();
    let authenticator = member
        .session
        .get_epoch_authenticator()
        .unwrap()
        .as_slice()
        .to_vec();
    let response = handle_json(
        &mut member,
        24,
        json!({ "epoch": 2, "protocol_version": 1 }),
    )
    .unwrap();
    assert!(response.is_empty());
    assert!(member.pending_transitions.is_empty());
    assert_eq!(member.epoch(), Some(2));
    assert_eq!(
        member.session.get_epoch_authenticator().unwrap().as_slice(),
        authenticator
    );
    assert!(member.is_ready());
    assert_encrypted_roundtrip(&mut member, &mut receiver);
    assert!(matches!(
        handle_json(&mut member, 22, json!({ "transition_id": 12 })),
        Err(VoiceError::Protocol(_))
    ));

    let response = handle_json(
        &mut member,
        21,
        json!({ "transition_id": 12, "protocol_version": 1 }),
    )
    .unwrap();
    assert_ready_ack(&response, 12);
    execute(&mut member, 12);
    assert_eq!(member.epoch(), Some(2));
    assert!(member.is_ready());
    assert_encrypted_roundtrip(&mut member, &mut receiver);
    assert!(matches!(
        handle_json(&mut member, 22, json!({ "transition_id": 12 })),
        Err(VoiceError::Protocol(_))
    ));
}

#[test]
fn future_epoch_waits_for_commit_and_welcome_transition_ids() {
    let (gateway, mut member, mut old_receiver) = established_group();
    let old_authenticator = member
        .session
        .get_epoch_authenticator()
        .unwrap()
        .as_slice()
        .to_vec();
    assert!(
        handle_json(
            &mut member,
            24,
            json!({ "epoch": 3, "protocol_version": 1 })
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(member.epoch(), Some(2));
    assert!(member.pending_transitions.is_empty());
    assert!(member.is_ready());

    let mut joiner = gateway.member(103);
    let result = gateway.add(&mut member, &mut joiner);
    assert_ready_ack(&handle_mls(&mut member, 29, 42, &result.commit), 42);
    assert_ready_ack(
        &handle_mls(&mut joiner, 30, 42, &result.welcome.unwrap()),
        42,
    );
    assert_eq!(member.epoch(), Some(3));
    assert_eq!(joiner.epoch(), Some(3));
    assert_ne!(
        member.session.get_epoch_authenticator().unwrap().as_slice(),
        old_authenticator
    );
    assert_eq!(
        member.session.get_epoch_authenticator().unwrap().as_slice(),
        joiner.session.get_epoch_authenticator().unwrap().as_slice()
    );
    assert!(!joiner.is_ready());
    assert!(matches!(
        joiner.encrypt_opus(&[1, 2, 3]),
        Err(VoiceError::DaveNotReady)
    ));
    // A processed commit must not switch sending keys before opcode 22.
    assert_encrypted_roundtrip(&mut member, &mut old_receiver);
    execute(&mut member, 42);
    execute(&mut joiner, 42);
    assert!(member.is_ready());
    assert!(joiner.is_ready());
    assert_encrypted_roundtrip(&mut member, &mut joiner);
    assert!(matches!(
        handle_json(
            &mut member,
            24,
            json!({ "epoch": 2, "protocol_version": 1 })
        ),
        Err(VoiceError::Protocol(_))
    ));
}

#[test]
fn established_group_reset_preserves_sole_member_initialization() {
    let (_, mut member, _) = established_group();
    assert_ready_ack(
        &handle_json(
            &mut member,
            21,
            json!({ "transition_id": 50, "protocol_version": 1 }),
        )
        .unwrap(),
        50,
    );
    let response = handle_json(
        &mut member,
        24,
        json!({ "epoch": 1, "protocol_version": 1 }),
    )
    .unwrap();
    assert!(
        matches!(response.as_slice(), [OutgoingGatewayMessage::Binary(bytes)] if bytes.first() == Some(&26))
    );
    assert_eq!(member.epoch(), Some(0));
    assert!(member.pending_transitions.is_empty());
    assert!(
        handle_json(
            &mut member,
            21,
            json!({ "transition_id": 0, "protocol_version": 1 })
        )
        .unwrap()
        .is_empty()
    );
    assert!(!member.is_ready());
    assert!(matches!(
        member.encrypt_opus(&[1, 2, 3]),
        Err(VoiceError::DaveNotReady)
    ));
    assert!(matches!(
        handle_json(&mut member, 22, json!({ "transition_id": 50 })),
        Err(VoiceError::Protocol(_))
    ));
}

#[test]
fn epoch_announcements_keep_strict_field_version_and_epoch_validation() {
    let (_, mut member, _) = established_group();
    for payload in [
        json!({ "protocol_version": 1 }),
        json!({ "epoch": 2 }),
        json!({ "epoch": -1, "protocol_version": 1 }),
        json!({ "epoch": 2.5, "protocol_version": 1 }),
        json!({ "epoch": 0, "protocol_version": 1 }),
    ] {
        assert!(matches!(
            handle_json(&mut member, 24, payload),
            Err(VoiceError::Protocol(_))
        ));
    }
    assert!(matches!(
        handle_json(
            &mut member,
            24,
            json!({ "epoch": 3, "protocol_version": 0 })
        ),
        Err(VoiceError::DaveRequired)
    ));
    assert!(matches!(
        handle_json(
            &mut member,
            24,
            json!({ "epoch": 3, "protocol_version": 2 })
        ),
        Err(VoiceError::UnsupportedDaveVersion(2))
    ));
    assert_eq!(member.epoch(), Some(2));
    assert!(member.pending_transitions.is_empty());
}

#[test]
fn transition_messages_still_require_transition_ids() {
    let mut member = DaveProtocol::new(1, 100, 200).unwrap();
    for opcode in [21, 22] {
        assert!(matches!(
            handle_json(&mut member, opcode, json!({ "protocol_version": 1 })),
            Err(VoiceError::Protocol(_))
        ));
    }
    assert!(member.pending_transitions.is_empty());
    assert!(!member.is_ready());
}
