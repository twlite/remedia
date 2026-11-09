use super::*;

#[test]
fn rejects_unsupported_input_schemes_before_network_access() {
    assert!(matches!(
        AudioDecoder::open(InputSource::http_url("ftp://example.com/audio.mp3")),
        Err(DecoderError::UnsupportedUrlScheme)
    ));
}

#[test]
fn reports_connection_failures_as_network_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.shutdown(Shutdown::Both).unwrap();
    });

    let result = AudioDecoder::open_with_options(
        InputSource::http_url(format!("http://{address}/audio.wav")),
        DecoderOptions {
            io_timeout: Duration::from_secs(2),
            cancellation: CancellationToken::new(),
        },
    );
    server.join().unwrap();

    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("the peer closed before returning an HTTP response"),
    };
    assert!(
        matches!(error, DecoderError::NetworkFailure(_)),
        "expected a network error after the peer closed the connection, got {error:?}"
    );
}

#[test]
fn follows_http_redirects_and_decodes_chunked_unknown_length_responses() {
    let input = pcm16_wave(48_000, &[vec![4096; 2400]]);
    let (url, server) = serve_redirect_and_chunked_body(input);
    let mut decoder = AudioDecoder::open(InputSource::http_url(url)).unwrap();

    assert_eq!(decoder.info().sample_rate, Some(48_000));
    let frames = collect_frames(&mut decoder);
    assert_eq!(frame_count(&frames), 2400);
    server.join().unwrap();
}

#[test]
fn http_status_failures_are_typed() {
    let (url, server) = serve_not_found();
    assert!(matches!(
        AudioDecoder::open(InputSource::http_url(url)),
        Err(DecoderError::HttpStatus(404))
    ));
    server.join().unwrap();
}

#[test]
fn cancellation_interrupts_a_blocked_http_open() {
    let (url, server, request_seen) = serve_stalled_response();
    let cancellation = CancellationToken::new();
    let decode_cancellation = cancellation.clone();
    let decoder_thread = thread::spawn(move || {
        AudioDecoder::open_with_options(
            InputSource::http_url(url),
            DecoderOptions {
                io_timeout: Duration::from_secs(10),
                cancellation: decode_cancellation,
            },
        )
    });

    request_seen
        .recv_timeout(Duration::from_secs(2))
        .expect("server received request before cancellation");
    let started = Instant::now();
    cancellation.cancel();
    assert!(matches!(
        decoder_thread.join().unwrap(),
        Err(DecoderError::Cancelled)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
}

#[test]
fn network_reads_obey_the_configured_timeout() {
    let (url, server, request_seen) = serve_stalled_response();
    let decoder_thread = thread::spawn(move || {
        AudioDecoder::open_with_options(
            InputSource::http_url(url),
            DecoderOptions {
                io_timeout: Duration::from_millis(50),
                cancellation: CancellationToken::new(),
            },
        )
    });
    request_seen
        .recv_timeout(Duration::from_secs(2))
        .expect("server received request before timeout");
    let result = decoder_thread.join().unwrap();

    assert!(matches!(result, Err(DecoderError::InputTimeout)));
    server.join().unwrap();
}
