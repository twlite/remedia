use super::*;

#[tokio::test]
async fn websocket_auth_request_correlation_and_malformed_message_errors() {
    let server = RunningServer::start(60, 4).await;
    let unauthenticated = server.connect("bot-a", "wrong").await.unwrap_err();
    match unauthenticated {
        WsError::Http(response) => assert_eq!(response.status(), 401),
        error => panic!("expected a rejected handshake, received {error}"),
    }
    let missing_bot_id = server
        .connect_without_bot_id("bot-a", "password-a")
        .await
        .unwrap_err();
    match missing_bot_id {
        WsError::Http(response) => assert_eq!(response.status(), 400),
        error => panic!("expected a rejected bot identity, received {error}"),
    }
    let invalid_bot_id = server
        .connect_as("bot-a", "password-a", "0")
        .await
        .unwrap_err();
    match invalid_bot_id {
        WsError::Http(response) => assert_eq!(response.status(), 400),
        error => panic!("expected a rejected bot identity, received {error}"),
    }

    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    let response = rpc(&mut socket, "correlation-42", "node.info", json!({})).await;
    assert_eq!(response["id"], "correlation-42");
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["protocolVersion"], 1);

    socket.send(Message::Text("not-json".into())).await.unwrap();
    let malformed = socket.next().await.unwrap().unwrap();
    let Message::Text(malformed) = malformed else {
        panic!("expected a JSON error response")
    };
    let malformed: Value = serde_json::from_str(malformed.as_str()).unwrap();
    assert_eq!(malformed["id"], Value::Null);
    assert_eq!(malformed["error"]["code"], "invalid_request");

    let sessions = rpc(&mut socket, "create", "client.create", json!({})).await;
    assert_eq!(sessions["ok"], true);
    let bad_params = rpc(
        &mut socket,
        "bad-params",
        "node.info",
        json!({"unexpected": true}),
    )
    .await;
    assert_eq!(bad_params["error"]["code"], "invalid_request");
    drop(socket);
    server.shutdown().await;
}

#[tokio::test]
async fn authenticated_connections_supply_independent_bot_ids_in_the_handshake() {
    let server = RunningServer::start(60, 4).await;
    let mut first = server
        .connect_as("bot-a", "password-a", BOT_A_ID)
        .await
        .unwrap();
    let first_session = rpc(&mut first, "first-create", "client.create", json!({})).await;
    assert_eq!(first_session["result"]["botUserId"], BOT_A_ID);
    let first_guild = rpc(
        &mut first,
        "first-guild",
        "guild.create",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(first_guild["result"]["botUserId"], BOT_A_ID);

    let mut second = server
        .connect_as("bot-a", "password-a", BOT_B_ID)
        .await
        .unwrap();
    let second_session = rpc(&mut second, "second-create", "client.create", json!({})).await;
    assert_eq!(second_session["result"]["botUserId"], BOT_B_ID);
    let second_guild = rpc(
        &mut second,
        "second-guild",
        "guild.create",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(second_guild["result"]["botUserId"], BOT_B_ID);
    let first_list = rpc(&mut first, "first-list", "client.list", json!({})).await;
    assert_eq!(
        first_list["result"]["sessions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        first_list["result"]["sessions"][0]["sessionId"],
        first_session["result"]["sessionId"]
    );
    let second_list = rpc(&mut second, "second-list", "client.list", json!({})).await;
    assert_eq!(
        second_list["result"]["sessions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        second_list["result"]["sessions"][0]["sessionId"],
        second_session["result"]["sessionId"]
    );

    let mut owner = server
        .connect_as("bot-a", "password-a", BOT_A_ID)
        .await
        .unwrap();
    let (session_id, resume_credential) = create_client(&mut owner).await;
    let mut wrong_bot = server
        .connect_as("bot-a", "password-a", BOT_B_ID)
        .await
        .unwrap();
    let resume = rpc(
        &mut wrong_bot,
        "wrong-bot-resume",
        "client.resume",
        json!({"sessionId": session_id, "resumeCredential": resume_credential}),
    )
    .await;
    assert_eq!(resume["error"]["code"], "unknown_session");

    drop(first);
    drop(second);
    drop(owner);
    drop(wrong_bot);
    server.shutdown().await;
}
