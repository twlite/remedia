use super::*;

#[tokio::test]
async fn client_and_guild_ownership_are_isolated_and_capacity_is_enforced() {
    let server = RunningServer::start(60, 1).await;
    let mut first = server.connect("bot-a", "password-a").await.unwrap();
    let (session_id, _) = create_client(&mut first).await;
    let created = rpc(
        &mut first,
        "guild-a",
        "guild.create",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    let limited = rpc(
        &mut first,
        "guild-limit",
        "guild.create",
        json!({"guildId": "200000000000000002"}),
    )
    .await;
    assert_eq!(limited["error"]["code"], "resource_exhausted");

    let mut second = server
        .connect_as("bot-b", "password-b", BOT_B_ID)
        .await
        .unwrap();
    let unauthorized_resume = rpc(
        &mut second,
        "resume-isolation",
        "client.resume",
        json!({"sessionId": session_id, "resumeCredential": "not-the-credential"}),
    )
    .await;
    assert_eq!(unauthorized_resume["error"]["code"], "unknown_session");
    let no_session = rpc(
        &mut second,
        "guild-isolation",
        "guild.get",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(no_session["error"]["code"], "unknown_session");

    create_client(&mut second).await;
    let second_guild = rpc(
        &mut second,
        "same-guild",
        "guild.create",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(second_guild["ok"], true, "{second_guild}");
    assert_ne!(
        created["result"]["guildSessionId"],
        second_guild["result"]["guildSessionId"]
    );
    let duplicate = rpc(
        &mut first,
        "duplicate",
        "guild.create",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(duplicate["error"]["code"], "invalid_state");
    let configured = rpc(
        &mut first,
        "gain",
        "player.volume.set",
        json!({"guildId": "200000000000000001", "gain": 0.4}),
    )
    .await;
    assert_eq!(configured["ok"], true, "{configured}");
    let independent = rpc(
        &mut second,
        "independent",
        "guild.get",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(independent["result"]["playback"]["volume"], 1.0);
    let close = rpc(&mut first, "close-client", "client.close", json!({})).await;
    assert_eq!(close["ok"], true, "{close}");
    let stats = rpc(&mut second, "stats", "node.stats", json!({})).await;
    assert_eq!(stats["result"]["players"]["active"], 1);
    let destroyed = rpc(
        &mut second,
        "destroy",
        "guild.destroy",
        json!({"guildId": "200000000000000001"}),
    )
    .await;
    assert_eq!(destroyed["ok"], true, "{destroyed}");
    let stats = rpc(&mut second, "clean", "node.stats", json!({})).await;
    assert_eq!(stats["result"]["players"]["active"], 0);
    drop(first);
    drop(second);
    server.shutdown().await;
}
