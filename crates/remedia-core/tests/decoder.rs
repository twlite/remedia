use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use remedia_core::{
    AudioDecoder, CancellationToken, DecoderError, DecoderOptions, InputSource, PcmFormat,
};
use remedia_observability::{DiagnosticValue, Observability, ObservabilityConfig};

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

fn pcm16_wave_header(sample_rate: u32, channel_count: u16, data_len: usize) -> Vec<u8> {
    let block_align = channel_count * 2;
    let mut bytes = Vec::with_capacity(44);
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
    // Consume the complete request before replying. Closing a TCP socket with
    // unread request headers can send RST instead of FIN, truncating the
    // chunked response as seen by FFmpeg 9's HTTP demuxer.
    loop {
        let mut header = String::new();
        let bytes = reader.read_line(&mut header).unwrap();
        if bytes == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }
    request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

#[path = "decoder/decoding.rs"]
mod decoding;
#[path = "decoder/network.rs"]
mod network;
#[path = "decoder/streams.rs"]
mod streams;
