use super::*;

/// What a single-use media endpoint observed.
#[derive(Default)]
struct Observed {
    requests: AtomicUsize,
    ranged_requests: AtomicUsize,
}

/// Serve `body` like a single-use grant: a plain `GET` returns the complete
/// body once, any `Range` request is answered with 416, and later requests 404.
fn serve_single_use_grant(body: Vec<u8>, content_type: &'static str) -> (String, Arc<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let observed = Arc::new(Observed::default());
    let server_observed = Arc::clone(&observed);

    thread::spawn(move || {
        let mut redeemed = false;

        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();

            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }

            let mut ranged = false;

            loop {
                let mut header = String::new();
                let bytes = reader.read_line(&mut header).unwrap_or(0);

                if bytes == 0 || header == "\r\n" || header == "\n" {
                    break;
                }

                ranged |= header.to_ascii_lowercase().starts_with("range:");
            }

            server_observed.requests.fetch_add(1, Ordering::SeqCst);

            if ranged {
                server_observed
                    .ranged_requests
                    .fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(
                    b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );

                continue;
            }

            if redeemed {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );

                continue;
            }

            redeemed = true;

            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });

    (format!("http://{address}/remedia/v1/media/token"), observed)
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("fixture is readable")
}

#[test]
fn ordinary_http_input_asks_for_a_range_that_a_single_use_grant_rejects() {
    let (url, observed) = serve_single_use_grant(fixture("generated-tone.mp3"), "audio/mpeg");
    let result = AudioDecoder::open(InputSource::http_url(url));

    assert!(
        result.is_err(),
        "FFmpeg's default Range request is rejected with 416"
    );
    assert!(observed.ranged_requests.load(Ordering::SeqCst) >= 1);
}

#[test]
fn sequential_http_input_decodes_mp3_with_one_unranged_request() {
    let (url, observed) = serve_single_use_grant(fixture("generated-tone.mp3"), "audio/mpeg");
    let mut decoder = AudioDecoder::open(InputSource::sequential_http_url(url)).unwrap();

    assert_eq!(decoder.info().codec, "mp3");
    assert!(frame_count(&collect_frames(&mut decoder)) > 0);
    drop(decoder);
    assert_eq!(observed.requests.load(Ordering::SeqCst), 1);
    assert_eq!(observed.ranged_requests.load(Ordering::SeqCst), 0);
}

#[test]
fn sequential_http_input_decodes_ogg_opus_without_reopening() {
    let (url, observed) =
        serve_single_use_grant(fixture("generated-opus-passthrough.ogg"), "audio/ogg");
    let mut decoder = AudioDecoder::open(InputSource::sequential_http_url(url)).unwrap();

    assert_eq!(decoder.info().codec, "opus");
    assert!(frame_count(&collect_frames(&mut decoder)) > 0);
    drop(decoder);
    assert_eq!(observed.requests.load(Ordering::SeqCst), 1);
    assert_eq!(observed.ranged_requests.load(Ordering::SeqCst), 0);
}

#[test]
fn sequential_http_input_seeks_forward_but_cannot_rewind_or_reopen() {
    let (url, observed) = serve_single_use_grant(fixture("generated-tone.mp3"), "audio/mpeg");
    let mut decoder = AudioDecoder::open(InputSource::sequential_http_url(url)).unwrap();

    decoder.seek(Duration::from_millis(100)).unwrap();
    assert!(decoder.next_frame().unwrap().is_some());
    assert!(matches!(
        decoder.seek(Duration::ZERO),
        Err(DecoderError::UnsupportedSeeking(_))
    ));
    drop(decoder);
    assert_eq!(observed.requests.load(Ordering::SeqCst), 1);
    assert_eq!(observed.ranged_requests.load(Ordering::SeqCst), 0);
}

#[test]
fn sequential_http_input_reports_an_already_consumed_grant_as_http_status() {
    let (url, _observed) = serve_single_use_grant(fixture("generated-tone.mp3"), "audio/mpeg");

    drop(AudioDecoder::open(InputSource::sequential_http_url(url.clone())).unwrap());
    assert!(matches!(
        AudioDecoder::open(InputSource::sequential_http_url(url)),
        Err(DecoderError::HttpStatus(404))
    ));
}

#[test]
fn sequential_http_input_validates_urls_like_ordinary_http_input() {
    assert!(matches!(
        AudioDecoder::open(InputSource::sequential_http_url("file:///etc/passwd")),
        Err(DecoderError::UnsupportedUrlScheme)
    ));
    assert_eq!(
        InputSource::sequential_http_url("http://127.0.0.1/a").kind(),
        "http_sequential"
    );
    assert_ne!(
        InputSource::sequential_http_url("http://127.0.0.1/a"),
        InputSource::http_url("http://127.0.0.1/a")
    );
}
