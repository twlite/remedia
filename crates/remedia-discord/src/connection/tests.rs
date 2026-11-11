use super::*;
use remedia_core::CancellationToken;

#[test]
fn mock_packet_barriers_wait_for_target_without_blocking_other_channels() {
    for finish in [false, true] {
        let voice = TestVoiceConnection::new(42);
        let (ack, result) = mpsc::sync_channel(1);
        let control = if finish {
            Control::Finish { target_id: 2, ack }
        } else {
            Control::Pause { target_id: 2, ack }
        };
        voice.connection.inner.control_tx.send(control).unwrap();

        // A later control acknowledgement proves the actor observed the
        // barrier before any packets, and remained able to receive controls.
        let (ack, resumed) = mpsc::sync_channel(1);
        voice
            .connection
            .inner
            .control_tx
            .send(Control::Resume { ack })
            .unwrap();
        resumed
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));

        for id in 1..=2 {
            voice
                .connection
                .inner
                .packet_tx
                .send(QueuedPacket {
                    generation: 0,
                    id,
                    packet: EncodedOpusPacket {
                        bytes: SILENCE_PACKET.to_vec(),
                        samples_per_channel: 960,
                        media_samples_per_channel: 960,
                        start_sample: (id - 1) * 960,
                    },
                    played_end_sample: Arc::new(AtomicU64::new(u64::MAX)),
                })
                .unwrap();
            voice.wait_for_packets(id as usize);
            if id == 1 {
                assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
            }
        }
        result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
    }
}

#[test]
fn endpoint_is_secure_and_pins_voice_gateway_v8() {
    let endpoint = voice_endpoint("voice.example.test:443").unwrap();
    assert_eq!(endpoint.scheme(), "wss");
    assert!(
        endpoint
            .query_pairs()
            .any(|(key, value)| key == "v" && value == "8")
    );
    assert!(
        endpoint
            .query_pairs()
            .any(|(key, value)| key == "encoding" && value == "json")
    );
    assert!(voice_endpoint("ws://voice.example.test").is_err());
}

#[test]
fn close_codes_resume_only_when_discord_allows_it() {
    assert_eq!(voice_close_policy(4015), VoiceClosePolicy::Resume);
    assert_eq!(voice_close_policy(1006), VoiceClosePolicy::Resume);
    assert_eq!(voice_close_policy(4006), VoiceClosePolicy::FreshIdentify);
    assert_eq!(voice_close_policy(4009), VoiceClosePolicy::FreshIdentify);
    assert_eq!(voice_close_policy(4014), VoiceClosePolicy::WaitForGateway);
    assert_eq!(voice_close_policy(4021), VoiceClosePolicy::WaitForGateway);
    assert_eq!(voice_close_policy(4022), VoiceClosePolicy::WaitForGateway);
}

#[test]
fn connection_debug_redacts_voice_credentials() {
    let info = VoiceConnectionInfo {
        guild_id: 1,
        channel_id: 2,
        user_id: 3,
        session_id: "private-session-value".into(),
        token: "private-token-value".into(),
        endpoint: "voice.example.test/?token=private-endpoint-value".into(),
    };
    let debug = format!("{info:?}");
    assert!(!debug.contains("private-session-value"));
    assert!(!debug.contains("private-token-value"));
    assert!(!debug.contains("private-endpoint-value"));
    assert!(debug.contains("[redacted]"));
}

#[test]
fn parses_ready_fields_and_rejects_invalid_secret_key_sizes() {
    let ready = parse_ready(&json!({
        "ssrc": 0x1020,
        "ip": "192.0.2.5",
        "port": 4433,
        "modes": ["aead_xchacha20_poly1305_rtpsize", "xsalsa20_poly1305_lite"]
    }))
    .unwrap();
    assert_eq!(ready.ssrc, 0x1020);
    assert_eq!(ready.ip, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5)));
    assert_eq!(ready.port, 4433);
    assert_eq!(ready.modes.len(), 2);
    assert!(matches!(
        parse_secret_key(&json!({ "secret_key": [0, 1, 2] })),
        Err(VoiceError::InvalidSessionKey)
    ));
}

#[tokio::test]
async fn udp_discovery_reports_external_address_and_keeps_socket_open() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let mut request = [0; 74];
        let (_, client) = server.recv_from(&mut request).await.unwrap();
        let ssrc = u32::from_be_bytes(request[4..8].try_into().unwrap());
        let mut response = [0; 74];
        response[..2].copy_from_slice(&2_u16.to_be_bytes());
        response[2..4].copy_from_slice(&70_u16.to_be_bytes());
        response[4..8].copy_from_slice(&ssrc.to_be_bytes());
        response[8..20].copy_from_slice(b"203.0.113.8\0");
        response[72..74].copy_from_slice(&55123_u16.to_be_bytes());
        server.send_to(&response, client).await.unwrap();
    });

    let ready = VoiceReady {
        ssrc: 0x1020_3040,
        ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: server_addr.port(),
        modes: vec!["aead_xchacha20_poly1305_rtpsize".into()],
    };
    let (socket, external) = establish_udp(&ready).await.unwrap();
    assert_eq!(external, ("203.0.113.8".parse().unwrap(), 55123));
    assert_ne!(socket.local_addr().unwrap().port(), 0);
    server_task.await.unwrap();
}

#[tokio::test]
async fn voice_handshake_uses_dave_v8_and_negotiates_udp_with_mock_endpoints() {
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    let udp_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_address = udp_server.local_addr().unwrap();
    let discovery_task = tokio::spawn(async move {
        let mut request = [0; 74];
        let (_, client) = udp_server.recv_from(&mut request).await.unwrap();
        let ssrc = u32::from_be_bytes(request[4..8].try_into().unwrap());
        let mut response = [0; 74];
        response[..2].copy_from_slice(&2_u16.to_be_bytes());
        response[2..4].copy_from_slice(&70_u16.to_be_bytes());
        response[4..8].copy_from_slice(&ssrc.to_be_bytes());
        response[8..20].copy_from_slice(b"203.0.113.8\0");
        response[72..74].copy_from_slice(&55123_u16.to_be_bytes());
        udp_server.send_to(&response, client).await.unwrap();
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_address = listener.local_addr().unwrap();
    let gateway_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = accept_async(stream).await.unwrap();
        let identify = websocket.next().await.unwrap().unwrap();
        let identify: Value = serde_json::from_str(identify.into_text().unwrap().as_str()).unwrap();
        assert_eq!(identify["op"], 0);
        assert_eq!(
            identify["d"]["max_dave_protocol_version"],
            DAVE_PROTOCOL_VERSION
        );

        websocket
            .send(Message::Text(
                json!({ "op": 8, "d": { "heartbeat_interval": 1_000 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        websocket
            .send(Message::Text(
                json!({
                    "op": 2,
                    "d": {
                        "ssrc": 0x1020_3040_u32,
                        "ip": "127.0.0.1",
                        "port": udp_address.port(),
                        "modes": ["aead_xchacha20_poly1305_rtpsize"],
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let select = websocket.next().await.unwrap().unwrap();
        let select: Value = serde_json::from_str(select.into_text().unwrap().as_str()).unwrap();
        assert_eq!(select["op"], 1);
        assert_eq!(
            select["d"]["data"]["mode"],
            "aead_xchacha20_poly1305_rtpsize"
        );

        websocket
            .send(Message::Text(
                json!({ "op": 11, "d": { "user_ids": ["42"] }, "seq": 8 })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        websocket
            .send(Message::Text(
                json!({
                    "op": 4,
                    "d": {
                        "mode": "aead_xchacha20_poly1305_rtpsize",
                        "secret_key": vec![0_u8; 32],
                        "dave_protocol_version": DAVE_PROTOCOL_VERSION,
                    },
                    "seq": 9,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    });

    let endpoint = Url::parse(&format!("ws://{ws_address}/")).unwrap();
    let info = VoiceConnectionInfo {
        guild_id: 10,
        channel_id: 20,
        user_id: 30,
        session_id: "test-session".into(),
        token: "test-token".into(),
        endpoint: ws_address.to_string(),
    };
    let (websocket, interval, ready, early, session, udp, sequence_ack) =
        establish_voice_session(&endpoint, &info, 1).await.unwrap();
    assert_eq!(interval, Duration::from_secs(1));
    assert_eq!(ready.ssrc, 0x1020_3040);
    assert_eq!(early.len(), 1);
    assert_eq!(sequence_ack, Some(9));
    assert_eq!(session["dave_protocol_version"], DAVE_PROTOCOL_VERSION);
    assert!(udp.local_addr().unwrap().port() > 0);
    drop(websocket);
    gateway_task.await.unwrap();
    discovery_task.await.unwrap();
}

#[test]
fn packet_pacing_keeps_deadlines_and_rebases_late_frames_without_catchup() {
    let start = Instant::now();
    let frame = Duration::from_millis(20);
    assert_eq!(
        packet_deadline(start, start + Duration::from_millis(5), frame),
        Some(start + frame)
    );
    assert_eq!(
        packet_deadline(start, start + Duration::from_millis(21), frame),
        None
    );
    assert_eq!(
        paced_deadline(start, start + Duration::from_millis(35), frame),
        start + Duration::from_millis(55)
    );
}

#[test]
fn output_cancel_fallback_discards_stale_packets_and_is_idempotent() {
    let mut queued = VecDeque::from([QueuedPacket {
        generation: 3,
        id: 4,
        packet: EncodedOpusPacket {
            bytes: vec![1],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        },
        played_end_sample: Arc::new(AtomicU64::new(u64::MAX)),
    }]);
    let mut processed_id = 2;
    let mut barrier = None;
    let mut paused = true;
    let mut playback_active = true;
    let mut next_send_at = None;

    begin_output_cancel(
        &mut queued,
        &mut processed_id,
        &mut barrier,
        &mut paused,
        &mut playback_active,
        true,
        &mut next_send_at,
    );
    assert!(queued.is_empty());
    assert_eq!(processed_id, 4);
    assert!(!paused);
    assert!(!playback_active);
    assert!(next_send_at.is_some());
    let silence_packets = barrier.as_ref().unwrap().silence_packets;
    assert_eq!(silence_packets, 5);

    begin_output_cancel(
        &mut queued,
        &mut processed_id,
        &mut barrier,
        &mut paused,
        &mut playback_active,
        true,
        &mut next_send_at,
    );
    assert_eq!(barrier.as_ref().unwrap().silence_packets, silence_packets);
}

#[test]
fn new_output_packet_supersedes_an_old_cancel_barrier() {
    let deadline = Instant::now() + Duration::from_millis(20);
    let mut barrier = Some(PendingBarrier {
        target_id: 8,
        silence_packets: 4,
        kind: BarrierKind::Cancel,
        ack: None,
    });
    let mut paused = false;
    let mut playback_active = false;
    let mut next_send_at = Some(deadline);

    assert!(supersede_cancel_barrier(
        &mut barrier,
        &mut paused,
        &mut playback_active,
        &mut next_send_at,
    ));
    assert!(barrier.is_none());
    assert!(!paused);
    assert!(playback_active);
    assert_eq!(next_send_at, Some(deadline));
}

#[test]
fn coalesced_generation_update_does_not_reapply_an_old_cancel() {
    let cancelled_generation = AtomicU64::new(2);
    let mut active_generation = 1;

    // Cancellation published generation 2, then the replacement sender
    // published generation 3 before the actor observed the watch change.
    assert!(!synchronize_output_generation(
        3,
        &mut active_generation,
        &cancelled_generation,
    ));

    assert_eq!(active_generation, 3);
    assert!(!cancellation_matches_generation(
        1,
        3,
        &cancelled_generation
    ));
}

#[test]
fn current_cancel_generation_starts_exactly_one_cancel_barrier() {
    let cancelled_generation = AtomicU64::new(2);
    let mut active_generation = 1;
    let mut queued = VecDeque::from([QueuedPacket {
        generation: 1,
        id: 7,
        packet: EncodedOpusPacket {
            bytes: vec![1],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        },
        played_end_sample: Arc::new(AtomicU64::new(u64::MAX)),
    }]);
    let mut processed_id = 3;
    let mut barrier = None;
    let mut paused = false;
    let mut playback_active = true;
    let mut next_send_at = None;

    assert!(cancellation_matches_generation(1, 2, &cancelled_generation));
    assert!(synchronize_output_generation(
        2,
        &mut active_generation,
        &cancelled_generation,
    ));
    begin_output_cancel(
        &mut queued,
        &mut processed_id,
        &mut barrier,
        &mut paused,
        &mut playback_active,
        true,
        &mut next_send_at,
    );

    assert_eq!(active_generation, 2);
    assert!(queued.is_empty());
    assert_eq!(processed_id, 7);
    assert!(!playback_active);
    assert!(matches!(
        barrier.as_ref().map(|pending| &pending.kind),
        Some(BarrierKind::Cancel)
    ));
}

#[test]
fn concurrent_sender_replacement_and_drop_publish_the_latest_generation() {
    let (packet_tx, _packet_rx) = flume::bounded(QUEUE_CAPACITY);
    let (control_tx, _control_rx) = flume::bounded(16);
    let (generation_tx, generation_rx) = watch::channel(0);
    let (state, _) = watch::channel(ConnectionState::Connected);
    let connection = DiscordVoiceConnection {
        inner: Arc::new(ConnectionInner {
            voice_connection_id: 4,
            guild_id: 10,
            observability: ObservabilityHandle::default(),
            state,
            failure: Arc::new(Mutex::new(None)),
            packet_tx,
            control_tx,
            generation: AtomicU64::new(0),
            generation_update: Mutex::new(()),
            generation_tx,
            cancelled_generation: Arc::new(AtomicU64::new(u64::MAX)),
            accepted_packet_id: AtomicU64::new(0),
            next_packet_id: AtomicU64::new(0),
            cancellation: TokioCancellationToken::new(),
            worker: Mutex::new(None),
        }),
    };
    let gate = Arc::new(std::sync::Barrier::new(9));
    let workers = (0..8)
        .map(|_| {
            let connection = connection.clone();
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.wait();
                for _ in 0..100 {
                    drop(connection.opus_sender());
                }
            })
        })
        .collect::<Vec<_>>();

    gate.wait();
    for worker in workers {
        worker.join().unwrap();
    }

    assert_eq!(
        *generation_rx.borrow(),
        connection.inner.generation.load(Ordering::Acquire)
    );
}

#[tokio::test]
async fn cancellation_interrupts_a_resume_attempt() {
    let info = VoiceConnectionInfo {
        guild_id: 1,
        channel_id: 2,
        user_id: 3,
        session_id: "session".into(),
        token: "token".into(),
        endpoint: "voice.example.test".into(),
    };
    let cancellation = TokioCancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        resume_gateway_cancellable(&info, None, &cancellation).await,
        Err(VoiceError::Cancelled)
    ));
}

#[test]
fn heartbeat_ack_must_match_and_timeout_uses_elapsed_monotonic_time() {
    let now = Instant::now();
    let interval = Duration::from_secs(5);
    assert!(heartbeat_ack_matches(Some(7), Some(7)));
    assert!(!heartbeat_ack_matches(Some(7), Some(8)));
    assert!(!heartbeat_ack_matches(None, None));
    assert!(!heartbeat_expired(
        now,
        now + Duration::from_secs(10),
        interval
    ));
    assert!(heartbeat_expired(
        now,
        now + Duration::from_millis(10_001),
        interval
    ));
}

#[test]
fn opus_sender_backpressures_at_its_bounded_queue_and_cancellation_unblocks() {
    let (packet_tx, packet_rx) = flume::bounded(QUEUE_CAPACITY);
    let (control_tx, _control_rx) = flume::bounded(16);
    let (generation_tx, _generation_rx) = watch::channel(0);
    let (state, _) = watch::channel(ConnectionState::Connected);
    let inner = Arc::new(ConnectionInner {
        voice_connection_id: 1,
        guild_id: 10,
        observability: ObservabilityHandle::default(),
        state,
        failure: Arc::new(Mutex::new(None)),
        packet_tx,
        control_tx,
        generation: AtomicU64::new(0),
        generation_update: Mutex::new(()),
        generation_tx,
        cancelled_generation: Arc::new(AtomicU64::new(u64::MAX)),
        accepted_packet_id: AtomicU64::new(0),
        next_packet_id: AtomicU64::new(0),
        cancellation: TokioCancellationToken::new(),
        worker: Mutex::new(None),
    });
    let sender = Arc::new(DiscordOpusSender {
        inner,
        generation: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        played_end_sample: Arc::new(AtomicU64::new(u64::MAX)),
    });
    sender
        .inner
        .state
        .send_replace(ConnectionState::Reconnecting);
    let packet = || EncodedOpusPacket {
        bytes: vec![1],
        samples_per_channel: 960,
        media_samples_per_channel: 960,
        start_sample: 0,
    };
    let token = CancellationToken::new();
    assert_eq!(sender.played_position(), None);
    for _ in 0..QUEUE_CAPACITY {
        sender.send_packet(packet(), &token).unwrap();
    }
    // The bounded queue has accepted packets, but the RTP actor has not dispatched them.
    assert_eq!(sender.played_position(), None);
    sender.played_end_sample.store(960, Ordering::Release);
    assert_eq!(sender.played_position(), Some(Duration::from_millis(20)));

    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let blocked_sender = Arc::clone(&sender);
    let blocked_token = token.clone();
    let worker = std::thread::spawn(move || {
        let result = blocked_sender.send_packet(packet(), &blocked_token);
        let _ = result_tx.send(result);
    });
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_millis(60)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    token.cancel();
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(OutputError::Cancelled)
    ));
    worker.join().unwrap();
    assert_eq!(packet_rx.len(), QUEUE_CAPACITY);
    sender.cancel();
    assert_eq!(sender.inner.cancelled_generation.load(Ordering::Acquire), 1);
}
