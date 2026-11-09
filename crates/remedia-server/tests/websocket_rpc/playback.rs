use super::*;

#[tokio::test]
async fn real_local_media_playback_and_cleanup_use_the_core_engine() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    create_client(&mut socket).await;
    let guild = "200000000000000010";
    assert_eq!(
        rpc(
            &mut socket,
            "guild",
            "guild.create",
            json!({"guildId": guild})
        )
        .await["ok"],
        true
    );

    let invalid_voice_endpoint = rpc(
        &mut socket,
        "voice-endpoint-policy",
        "voice.connect",
        json!({
            "guildId": "200000000000000010",
            "channelId": "200000000000000011",
            "sessionId": "fixture-session",
            "token": "fixture-token",
            "endpoint": "127.0.0.1:8787"
        }),
    )
    .await;
    assert_eq!(invalid_voice_endpoint["error"]["code"], "invalid_request");

    let invalid_pause = rpc(
        &mut socket,
        "pause-idle",
        "player.pause",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(invalid_pause["error"]["code"], "invalid_state");
    let filter = rpc(
        &mut socket,
        "set-filter",
        "player.filter.set",
        json!({"guildId": guild, "graph": "bass=gain=6"}),
    )
    .await;
    assert_eq!(filter["ok"], true, "{filter}");
    let volume = rpc(
        &mut socket,
        "set-volume",
        "player.volume.set",
        json!({"guildId": guild, "gain": 0.75}),
    )
    .await;
    assert_eq!(volume["result"]["volume"], 0.75);
    let invalid_filter = rpc(
        &mut socket,
        "invalid-filter",
        "player.filter.set",
        json!({"guildId": guild, "graph": "not_a_real_filter"}),
    )
    .await;
    assert_eq!(invalid_filter["error"]["code"], "playback_failed");
    let configured_state = rpc(
        &mut socket,
        "configured-state",
        "player.state",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(configured_state["result"]["filterGraph"], "bass=gain=6");
    assert_eq!(configured_state["result"]["volume"], 0.75);

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../remedia-core/tests/fixtures/generated-opus-passthrough.ogg")
        .canonicalize()
        .unwrap();
    let played = rpc(
        &mut socket,
        "play-local",
        "player.play",
        json!({"guildId": guild, "source": {"type":"file", "path": fixture}}),
    )
    .await;
    assert_eq!(played["ok"], true, "{played}");
    assert_eq!(played["result"]["codec"], "opus");
    let playback_generation = played["result"]["generation"].as_u64().unwrap();
    assert!(playback_generation > 0);
    assert!(played["result"]["durationMs"].as_u64().unwrap() > 0);

    let state = rpc(
        &mut socket,
        "playback-state",
        "player.state",
        json!({"guildId": guild}),
    )
    .await;
    assert!(matches!(
        state["result"]["state"].as_str(),
        Some("playing" | "completed")
    ));
    assert_eq!(
        state["result"]["generation"].as_u64(),
        Some(playback_generation)
    );
    assert_eq!(state["result"]["filterGraph"], "bass=gain=6");
    assert_eq!(state["result"]["volume"], 0.75);
    let stopped = rpc(
        &mut socket,
        "stop",
        "player.stop",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    let clear_filter = rpc(
        &mut socket,
        "clear-filter",
        "player.filter.clear",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(clear_filter["ok"], true, "{clear_filter}");
    let current_volume = rpc(
        &mut socket,
        "current-volume",
        "player.state",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(current_volume["result"]["volume"], 0.75);

    let outside_root = rpc(
        &mut socket,
        "outside-file",
        "player.play",
        json!({"guildId": guild, "source": {"type":"file", "path": "/etc/hosts"}}),
    )
    .await;
    assert_eq!(outside_root["error"]["code"], "invalid_request");
    let destroyed = rpc(
        &mut socket,
        "destroy-guild",
        "guild.destroy",
        json!({"guildId": guild}),
    )
    .await;
    assert_eq!(destroyed["ok"], true, "{destroyed}");
    let closed = rpc(&mut socket, "close", "client.close", json!({})).await;
    assert_eq!(closed["ok"], true, "{closed}");
    drop(socket);
    server.shutdown().await;
}

#[tokio::test]
async fn lifecycle_events_include_stable_generations_and_keep_protocol_v1_volume_field() {
    let server = RunningServer::start(60, 4).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    create_client(&mut socket).await;
    let guild = "200000000000000012";
    assert_eq!(
        rpc(
            &mut socket,
            "event-guild",
            "guild.create",
            json!({"guildId": guild})
        )
        .await["ok"],
        true
    );

    let (volume_response, mut volume_events) = rpc_collect_events(
        &mut socket,
        "event-volume",
        "player.volume.set",
        json!({"guildId": guild, "gain": 0.75}),
    )
    .await;
    assert_eq!(volume_response["ok"], true, "{volume_response}");
    let volume_event = match volume_events
        .drain(..)
        .find(|event| event["event"] == "player.volume_changed")
    {
        Some(event) => event,
        None => next_event(&mut socket, "player.volume_changed").await,
    };
    assert_eq!(volume_event["data"]["volume"], 0.75);
    assert_eq!(volume_event["data"]["gain"], 0.75);

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../remedia-core/tests/fixtures/generated-opus-passthrough.ogg")
        .canonicalize()
        .unwrap();
    let (play_response, mut play_events) = rpc_collect_events(
        &mut socket,
        "event-play",
        "player.play",
        json!({"guildId": guild, "source": {"type":"file", "path": fixture}}),
    )
    .await;
    assert_eq!(play_response["ok"], true, "{play_response}");
    let started_event = match play_events
        .drain(..)
        .find(|event| event["event"] == "player.started")
    {
        Some(event) => event,
        None => next_event(&mut socket, "player.started").await,
    };
    assert_eq!(
        started_event["data"]["generation"],
        play_response["result"]["generation"]
    );
    drop(socket);
    server.shutdown().await;
}
