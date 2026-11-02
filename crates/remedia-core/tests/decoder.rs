use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use remedia_core::{
    AudioDecoder, CancellationToken, DecoderError, DecoderOptions, InputSource, PcmFormat,
};

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

struct TempFile {
    directory: PathBuf,
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str, contents: &[u8]) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("remedia-decoder-{}-{id}", std::process::id()));
        fs::create_dir_all(&directory).expect("create temporary test directory");
        let path = directory.join(name);
        fs::write(&path, contents).expect("write temporary test file");
        Self { directory, path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn converts_sample_rate_and_duplicates_mono_to_interleaved_stereo() {
    let input = pcm16_wave(22_050, &[vec![8192; 2205]]);
    let file = TempFile::new("音声.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert_eq!(decoder.output_format(), PcmFormat::NORMALIZED);
    assert_eq!(decoder.info().sample_rate, Some(22_050));
    assert_eq!(decoder.info().channels, Some(1));
    assert_eq!(decoder.info().duration, Some(Duration::from_millis(100)));

    let frames = collect_frames(&mut decoder);
    assert!(!frames.is_empty());
    assert_eq!(frame_count(&frames), 4800);

    let mut mono_output = Vec::new();
    for frame in &frames {
        assert_eq!(frame.samples.len(), frame.samples_per_channel * 2);
        let (pairs, remainder) = frame.samples.as_chunks::<2>();
        assert!(remainder.is_empty());
        for pair in pairs {
            assert_eq!(pair[0], pair[1], "mono should feed both stereo channels");
            assert!(pair[0].abs() <= 1.0);
            mono_output.push(pair[0]);
        }
    }
    assert!(
        mono_output[100..mono_output.len() - 100]
            .iter()
            .any(|sample| sample.abs() > 0.1)
    );
}

#[test]
fn preserves_left_right_order_when_interleaving_stereo_samples() {
    let input = pcm16_wave(48_000, &[vec![8192; 960], vec![-4096; 960]]);
    let file = TempFile::new("stereo.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();
    let frames = collect_frames(&mut decoder);

    assert_eq!(frame_count(&frames), 960);
    for pair in frames
        .iter()
        .flat_map(|frame| frame.samples.as_chunks::<2>().0)
    {
        assert!(pair[0] > 0.0, "left channel should remain first");
        assert!(pair[1] < 0.0, "right channel should remain second");
        assert!((pair[0] - 0.25).abs() < 0.01);
        assert!((pair[1] + 0.125).abs() < 0.01);
    }
}

#[test]
fn clamps_float_source_samples_to_the_normalized_range() {
    let input = float_wave(48_000, &[vec![2.0; 128], vec![-2.0; 128]]);
    let file = TempFile::new("out-of-range.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();
    let frames = collect_frames(&mut decoder);
    let samples = frames
        .iter()
        .flat_map(|frame| frame.samples.iter().copied())
        .collect::<Vec<_>>();

    assert_eq!(frame_count(&frames), 128);
    assert!(samples.contains(&1.0));
    assert!(samples.contains(&-1.0));
    assert!(samples.iter().all(|sample| (-1.0..=1.0).contains(sample)));
}

#[test]
fn downmixes_multichannel_audio_using_its_channel_layout() {
    let mut channels = vec![vec![0; 480]; 6];
    channels[2].fill(4096); // Front Center in the 5.1 channel mask below.
    let input = pcm16_wave_extensible(48_000, &channels, 0x3f);
    let file = TempFile::new("5point1.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert_eq!(decoder.info().channels, Some(6));
    assert!(decoder.info().channel_layout.is_some());
    let frames = collect_frames(&mut decoder);
    assert_eq!(frame_count(&frames), 480);

    let pairs = frames
        .iter()
        .flat_map(|frame| frame.samples.as_chunks::<2>().0)
        .collect::<Vec<_>>();
    assert!(pairs.iter().any(|pair| pair[0] > 0.05 && pair[1] > 0.05));
    assert!(pairs.iter().all(|pair| (pair[0] - pair[1]).abs() < 0.001));
}

#[test]
fn reports_sample_positions_and_stable_end_of_stream() {
    let input = pcm16_wave(48_000, &[vec![2048; 1920]]);
    let file = TempFile::new("eos.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    let mut expected_position = 0;
    while let Some(frame) = decoder.next_frame().unwrap() {
        assert_eq!(frame.start_sample, expected_position);
        assert_eq!(frame.samples.len(), frame.samples_per_channel * 2);
        expected_position += frame.samples_per_channel as u64;
    }
    assert_eq!(expected_position, 1920);
    assert!(decoder.next_frame().unwrap().is_none());
}

#[test]
fn seeks_forward_and_discards_audio_before_the_requested_position() {
    let input = pcm16_wave(48_000, &[vec![4096; 48_000]]);
    let file = TempFile::new("seek.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert!(decoder.next_frame().unwrap().is_some());
    decoder.seek(Duration::from_millis(500)).unwrap();
    let frames = collect_frames(&mut decoder);

    assert_eq!(frames.first().unwrap().start_sample, 24_000);
    assert_eq!(frame_count(&frames), 24_000);
}

#[test]
fn rejects_seek_positions_past_a_known_duration() {
    let input = pcm16_wave(48_000, &[vec![4096; 480]]);
    let file = TempFile::new("short-seek.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert!(matches!(
        decoder.seek(Duration::from_millis(20)),
        Err(DecoderError::SeekOutOfRange { .. })
    ));
}

#[test]
fn unsupported_or_missing_files_return_errors_without_panicking() {
    let missing = std::env::temp_dir().join("remedia-does-not-exist-音声.wav");
    assert!(matches!(
        AudioDecoder::open(InputSource::file(missing)),
        Err(DecoderError::SourceNotFound)
    ));

    let invalid = TempFile::new("not-a-media-file.bin", b"not media");
    assert!(matches!(
        AudioDecoder::open(InputSource::file(invalid.path.clone())),
        Err(DecoderError::UnsupportedMedia(_) | DecoderError::InvalidMedia(_))
    ));
}

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
    drop(listener);

    assert!(matches!(
        AudioDecoder::open_with_options(
            InputSource::http_url(format!("http://{address}/audio.wav")),
            DecoderOptions {
                io_timeout: Duration::from_secs(2),
                cancellation: CancellationToken::new(),
            }
        ),
        Err(DecoderError::NetworkFailure(_))
    ));
}

#[test]
fn video_only_inputs_report_no_audio_stream() {
    let y4m = b"YUV4MPEG2 W2 H2 F25:1 Ip A1:1 C420jpeg\nFRAME\n\x10\x10\x10\x10\x80\x80";
    let file = TempFile::new("video-only.y4m", y4m);

    assert!(matches!(
        AudioDecoder::open(InputSource::file(file.path.clone())),
        Err(DecoderError::NoAudioStream)
    ));
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

#[test]
fn concurrent_decoders_keep_their_state_independent() {
    let input = pcm16_wave(22_050, &[vec![4096; 1102]]);
    let file = TempFile::new("concurrent.wav", &input);
    let path = file.path.clone();
    let workers = (0..2)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || {
                let mut decoder = AudioDecoder::open(InputSource::file(path)).unwrap();
                frame_count(&collect_frames(&mut decoder))
            })
        })
        .collect::<Vec<_>>();

    let counts = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![2399, 2399]);
}

fn collect_frames(decoder: &mut AudioDecoder) -> Vec<remedia_core::PcmFrame> {
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        frames.push(frame);
    }
    frames
}

fn frame_count(frames: &[remedia_core::PcmFrame]) -> usize {
    frames.iter().map(|frame| frame.samples_per_channel).sum()
}

fn pcm16_wave(sample_rate: u32, channels: &[Vec<i16>]) -> Vec<u8> {
    assert!(!channels.is_empty());
    let sample_count = channels[0].len();
    assert!(channels.iter().all(|channel| channel.len() == sample_count));
    let channel_count = channels.len() as u16;
    let block_align = channel_count * 2;
    let data_len = sample_count * usize::from(block_align);
    let mut bytes = Vec::with_capacity(44 + data_len);

    bytes.extend_from_slice(b"RIFF");
    push_u32(&mut bytes, (36 + data_len) as u32);
    bytes.extend_from_slice(b"WAVEfmt ");
    push_u32(&mut bytes, 16);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, channel_count);
    push_u32(&mut bytes, sample_rate);
    push_u32(&mut bytes, sample_rate * u32::from(block_align));
    push_u16(&mut bytes, block_align);
    push_u16(&mut bytes, 16);
    bytes.extend_from_slice(b"data");
    push_u32(&mut bytes, data_len as u32);

    for sample_index in 0..sample_count {
        for channel in channels {
            bytes.extend_from_slice(&channel[sample_index].to_le_bytes());
        }
    }
    bytes
}

fn pcm16_wave_extensible(sample_rate: u32, channels: &[Vec<i16>], channel_mask: u32) -> Vec<u8> {
    assert!(!channels.is_empty());
    let sample_count = channels[0].len();
    assert!(channels.iter().all(|channel| channel.len() == sample_count));
    let channel_count = channels.len() as u16;
    let block_align = channel_count * 2;
    let data_len = sample_count * usize::from(block_align);
    let mut bytes = Vec::with_capacity(68 + data_len);

    bytes.extend_from_slice(b"RIFF");
    push_u32(&mut bytes, (60 + data_len) as u32);
    bytes.extend_from_slice(b"WAVEfmt ");
    push_u32(&mut bytes, 40);
    push_u16(&mut bytes, 0xfffe);
    push_u16(&mut bytes, channel_count);
    push_u32(&mut bytes, sample_rate);
    push_u32(&mut bytes, sample_rate * u32::from(block_align));
    push_u16(&mut bytes, block_align);
    push_u16(&mut bytes, 16);
    push_u16(&mut bytes, 22);
    push_u16(&mut bytes, 16);
    push_u32(&mut bytes, channel_mask);
    bytes.extend_from_slice(&[
        1, 0, 0, 0, // PCM subformat GUID, first DWORD
        0, 0, // second WORD
        16, 0, // third WORD
        128, 0, 0, 170, 0, 56, 155, 113, // final eight bytes
    ]);
    bytes.extend_from_slice(b"data");
    push_u32(&mut bytes, data_len as u32);

    for sample_index in 0..sample_count {
        for channel in channels {
            bytes.extend_from_slice(&channel[sample_index].to_le_bytes());
        }
    }
    bytes
}

fn float_wave(sample_rate: u32, channels: &[Vec<f32>]) -> Vec<u8> {
    assert!(!channels.is_empty());
    let sample_count = channels[0].len();
    assert!(channels.iter().all(|channel| channel.len() == sample_count));
    let channel_count = channels.len() as u16;
    let block_align = channel_count * 4;
    let data_len = sample_count * usize::from(block_align);
    let mut bytes = Vec::with_capacity(44 + data_len);

    bytes.extend_from_slice(b"RIFF");
    push_u32(&mut bytes, (36 + data_len) as u32);
    bytes.extend_from_slice(b"WAVEfmt ");
    push_u32(&mut bytes, 16);
    push_u16(&mut bytes, 3);
    push_u16(&mut bytes, channel_count);
    push_u32(&mut bytes, sample_rate);
    push_u32(&mut bytes, sample_rate * u32::from(block_align));
    push_u16(&mut bytes, block_align);
    push_u16(&mut bytes, 32);
    bytes.extend_from_slice(b"data");
    push_u32(&mut bytes, data_len as u32);

    for sample_index in 0..sample_count {
        for channel in channels {
            bytes.extend_from_slice(&channel[sample_index].to_le_bytes());
        }
    }
    bytes
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn serve_redirect_and_chunked_body(body: Vec<u8>) -> (String, JoinHandle<()>) {
    let (listener, url) = local_listener("redirect");
    let server = thread::spawn(move || {
        let (mut redirect, _) = listener.accept().unwrap();
        assert_eq!(request_path(&redirect), "/redirect");
        redirect
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /audio\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();

        let (mut audio, _) = listener.accept().unwrap();
        assert_eq!(request_path(&audio), "/audio");
        audio
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        for chunk in body.chunks(512) {
            write!(audio, "{:X}\r\n", chunk.len()).unwrap();
            audio.write_all(chunk).unwrap();
            audio.write_all(b"\r\n").unwrap();
        }
        audio.write_all(b"0\r\n\r\n").unwrap();
    });
    (url, server)
}

fn serve_not_found() -> (String, JoinHandle<()>) {
    let (listener, url) = local_listener("missing");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = request_path(&stream);
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    (url, server)
}

fn serve_stalled_response() -> (String, JoinHandle<()>, Receiver<()>) {
    let (listener, url) = local_listener("stalled");
    let (request_seen_tx, request_seen_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = request_path(&stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: 10000\r\nConnection: close\r\n\r\nRIFF",
            )
            .unwrap();
        request_seen_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(300));
    });
    (url, server, request_seen_rx)
}

fn local_listener(path: &str) -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(false).unwrap();
    let address = listener.local_addr().unwrap();
    (listener, format!("http://{address}/{path}"))
}

fn request_path(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}
