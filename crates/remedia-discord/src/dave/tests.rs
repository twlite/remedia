use super::*;

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
fn prepare_epoch_starts_mls_and_prepares_later_epoch_transitions() {
    let mut session = DaveProtocol::new(davey::DAVE_PROTOCOL_VERSION, 100, 200).unwrap();
    let initial = session
        .handle(&GatewayMessage::Json {
            opcode: 24,
            data: json!({
                "transition_id": 0,
                "epoch": 1,
                "protocol_version": davey::DAVE_PROTOCOL_VERSION,
            }),
            sequence: None,
        })
        .unwrap();
    assert!(
        matches!(initial.as_slice(), [OutgoingGatewayMessage::Binary(bytes)] if bytes.first() == Some(&26))
    );

    let later = session.handle(&GatewayMessage::Json {
        opcode: 24,
        data: json!({
            "transition_id": 23,
            "epoch": 2,
            "protocol_version": davey::DAVE_PROTOCOL_VERSION,
        }),
        sequence: None,
    });
    assert!(matches!(later, Err(VoiceError::Protocol(_))));
}
