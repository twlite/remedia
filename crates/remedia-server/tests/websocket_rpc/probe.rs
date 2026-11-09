use super::*;

#[tokio::test]
async fn stateless_probe_enforces_media_policy_and_leaves_guilds_empty() {
    let server = RunningServer::start(2, 2).await;
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../remedia-core/tests/fixtures/generated-tone.mp3")
        .canonicalize()
        .unwrap();
    let params = json!({"source": {"type": "file", "path": file}});
    assert_eq!(
        rpc(&mut socket, "no-client", "media.probe", params.clone()).await["ok"],
        false
    );
    rpc(
        &mut socket,
        "create-probe-client",
        "client.create",
        json!({}),
    )
    .await;
    let info = rpc(&mut socket, "probe", "media.probe", params).await;
    assert_eq!(info["ok"], true, "{info}");
    assert!(info["result"]["codec"].as_str().unwrap().starts_with("mp3"));
    let guilds = rpc(&mut socket, "list-probe-guilds", "guild.list", json!({})).await;
    assert_eq!(guilds["result"]["guilds"], json!([]));
    for (id, source) in [
        ("file-denied", json!({"type":"file", "path":"/etc/passwd"})),
        (
            "url-denied",
            json!({"type":"url", "url":"http://127.0.0.1/audio"}),
        ),
        ("bytes-denied", json!({"type":"buffer", "bytes":[1,2]})),
    ] {
        let response = rpc(&mut socket, id, "media.probe", json!({"source":source})).await;
        assert_eq!(response["ok"], false, "{response}");
    }
    socket.close(None).await.unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_probe_interrupts_native_http_io_without_blocking_rpc() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = RunningServer::start_with_media_policy(2, 2, false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let http = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nContent-Type: audio/wav\r\n\r\nRIFF",
            )
            .await
            .unwrap();
        entered_tx.send(()).unwrap();
        // Input cleanup must close this disposable HTTP connection.
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut rest))
            .await
            .unwrap();
    });
    let mut socket = server.connect("bot-a", "password-a").await.unwrap();
    rpc(&mut socket, "create", "client.create", json!({})).await;
    socket.send(Message::Text(json!({"version":1,"id":"blocked-probe","command":"media.probe","params":{"source":{"type":"url","url":format!("http://{address}/audio")}}}).to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rpc(&mut socket, "health", "node.health", json!({})).await["ok"],
        true
    );
    assert_eq!(
        rpc(
            &mut socket,
            "cancel",
            "media.probe.cancel",
            json!({"requestId":"blocked-probe"})
        )
        .await["ok"],
        true
    );
    tokio::time::timeout(Duration::from_secs(4), http)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rpc(&mut socket, "guilds", "guild.list", json!({})).await["result"]["guilds"],
        json!([])
    );
    socket.close(None).await.unwrap();
    server.shutdown().await;
}
