use super::*;

#[tokio::test]
async fn disconnected_client_can_resume_and_expired_sessions_are_removed() {
    let server = RunningServer::start(1, 4).await;
    let mut first = server.connect("bot-a", "password-a").await.unwrap();
    let (session_id, resume_credential) = create_client(&mut first).await;
    let guild = rpc(
        &mut first,
        "guild",
        "guild.create",
        json!({"guildId": "200000000000000020"}),
    )
    .await;
    assert_eq!(guild["ok"], true);
    first.send(Message::Close(None)).await.unwrap();
    drop(first);

    let mut reconnected = server.connect("bot-a", "password-a").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let listed = rpc(&mut reconnected, "list", "client.list", json!({})).await;
        if listed["result"]["sessions"][0]["attached"] == false {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket did not detach"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let resumed = rpc(
        &mut reconnected,
        "resume",
        "client.resume",
        json!({"sessionId": session_id, "resumeCredential": resume_credential}),
    )
    .await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    assert_eq!(
        resumed["result"]["resumeCredential"], resume_credential,
        "resume credentials remain stable if the resume response is lost"
    );
    assert_eq!(
        resumed["result"]["snapshot"]["guilds"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let stale = rpc(
        &mut reconnected,
        "stale-resume",
        "client.resume",
        json!({"sessionId": resumed["result"]["sessionId"], "resumeCredential": resume_credential}),
    )
    .await;
    assert_eq!(stale["error"]["code"], "invalid_state");
    rpc(&mut reconnected, "close", "client.close", json!({})).await;

    let mut expiring = server.connect("bot-a", "password-a").await.unwrap();
    let (expiring_id, _) = create_client(&mut expiring).await;
    expiring.send(Message::Close(None)).await.unwrap();
    drop(expiring);
    let mut observer = server.connect("bot-a", "password-a").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let listed = rpc(&mut observer, "list-expiry", "client.list", json!({})).await;
        let exists = listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["sessionId"] == expiring_id);
        if !exists {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached session did not expire"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(reconnected);
    drop(observer);
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_commands_and_invalid_ids_return_typed_errors() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    let unknown = rpc(&mut socket, "unknown", "node.magic", json!({})).await;
    assert_eq!(unknown["error"]["code"], "invalid_request");
    let invalid_id = rpc(
        &mut socket,
        "invalid-guild",
        "guild.create",
        json!({"guildId": "1.2"}),
    )
    .await;
    assert_eq!(invalid_id["error"]["code"], "invalid_request");
    drop(socket);
    server.shutdown().await;
}

#[tokio::test]
async fn server_shutdown_closes_all_remaining_sessions() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    create_client(&mut socket).await;
    rpc(
        &mut socket,
        "guild",
        "guild.create",
        json!({"guildId": "200000000000000030"}),
    )
    .await;
    server.shutdown().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("server should close the socket after sending queued events");
}

#[allow(dead_code)]
fn _assert_tungstenite_client_support_is_present(_: tungstenite::protocol::Role) {}
