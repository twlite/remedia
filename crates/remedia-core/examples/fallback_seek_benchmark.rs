//! Measures fallback seeking: how long a replacement takes to prepare, whether playback keeps
//! delivering audio on time while it does, and what it costs in CPU.
//!
//! ```sh
//! cargo run --release -p remedia-core --example fallback_seek_benchmark -- ./media/long-track.ogg
//! ```
//!
//! The track is served over HTTP by an in-process origin that ignores `Range`, which is what
//! makes its decoder unable to seek. `--rate-mbit` limits the origin's bandwidth. Each target is
//! measured against the same track played from a local file, where a seek is a demuxer call,
//! so the two latencies can be compared. One JSON object is printed per measurement.
//!
//! The paced output behaves like a voice sender: it takes audio only while less than 200 ms of
//! it is queued, and counts an underrun whenever the queue is empty when the worker delivers.

use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use remedia_core::{
    CancellationToken, InputSource, MediaOutput, OutputError, PcmFrame, PlaybackSession,
};

/// Audio the output keeps queued ahead of the clock, like a voice sender's jitter buffer.
const QUEUE: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Pacing {
    start: Option<Instant>,
    submitted_samples: u64,
    frames: u64,
    underruns: u64,
    worst_underrun_ms: f64,
    last_write: Option<Instant>,
    worst_gap_ms: f64,
}

#[derive(Clone)]
struct PacedOutput(Arc<Mutex<Pacing>>);

impl PacedOutput {
    fn new() -> (Self, Arc<Mutex<Pacing>>) {
        let state = Arc::new(Mutex::new(Pacing::default()));

        (Self(Arc::clone(&state)), state)
    }
}

impl MediaOutput for PacedOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let wait = {
            let mut state = self.0.lock().unwrap();
            let now = Instant::now();
            let start = *state.start.get_or_insert(now);
            let queued_until = Duration::from_micros(state.submitted_samples * 1_000_000 / 48_000);
            let elapsed = now.duration_since(start);

            if state.frames > 0 {
                if let Some(previous) = state.last_write {
                    let gap = now.duration_since(previous).as_secs_f64() * 1_000.0;
                    state.worst_gap_ms = state.worst_gap_ms.max(gap);
                }
                if queued_until < elapsed {
                    let late = (elapsed - queued_until).as_secs_f64() * 1_000.0;
                    state.underruns += 1;
                    state.worst_underrun_ms = state.worst_underrun_ms.max(late);
                    // The queue ran dry, so the clock restarts from this frame.
                    state.start = Some(now);
                    state.submitted_samples = 0;
                }
            }
            state.submitted_samples += frame.samples_per_channel as u64;
            state.frames += 1;
            state.last_write = Some(now);

            let start = state.start.unwrap_or(now);
            let queued_until = Duration::from_micros(state.submitted_samples * 1_000_000 / 48_000);

            (start + queued_until).checked_duration_since(now + QUEUE)
        };

        if let Some(wait) = wait {
            let deadline = Instant::now() + wait;
            while Instant::now() < deadline {
                if cancellation.is_cancelled() {
                    return Err(OutputError::Cancelled);
                }
                thread::sleep(Duration::from_millis(1));
            }
        }

        Ok(())
    }

    fn flush(
        &self,
        _position: Duration,
        _cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut state = self.0.lock().unwrap();
        state.start = None;
        state.submitted_samples = 0;
        state.last_write = None;

        Ok(())
    }

    fn finish(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    fn cancel(&self) {}
}

struct Origin {
    url: String,
    connections: Arc<AtomicUsize>,
    bytes: Arc<AtomicU64>,
}

/// Serve `body` for every request, ignoring `Range`, at about `bytes_per_second`.
fn serve(body: Arc<Vec<u8>>, bytes_per_second: Option<u64>) -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the origin");
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let (thread_connections, thread_bytes) = (Arc::clone(&connections), Arc::clone(&bytes));

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let (body, connections, bytes) = (
                Arc::clone(&body),
                Arc::clone(&thread_connections),
                Arc::clone(&thread_bytes),
            );
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                connections.fetch_add(1, Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/ogg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let started = Instant::now();
                let mut sent = 0_u64;
                for chunk in body.chunks(64 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    sent += chunk.len() as u64;
                    bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    if let Some(rate) = bytes_per_second {
                        let due = Duration::from_secs_f64(sent as f64 / rate as f64);
                        if let Some(sleep) = due.checked_sub(started.elapsed()) {
                            thread::sleep(sleep);
                        }
                    }
                }
            });
        }
    });

    Origin {
        url: format!("http://{address}/track"),
        connections,
        bytes,
    }
}

/// User plus system CPU time of this process, in seconds.
fn cpu_seconds() -> f64 {
    #[repr(C)]
    #[derive(Default)]
    struct Timeval {
        seconds: i64,
        microseconds: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Usage {
        user: Timeval,
        system: Timeval,
        rest: [i64; 14],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Usage) -> i32;
    }
    let mut usage = Usage::default();
    // SAFETY: `usage` is a valid, correctly sized `rusage` and RUSAGE_SELF is 0.
    unsafe { getrusage(0, &mut usage) };

    usage.user.seconds as f64
        + usage.user.microseconds as f64 / 1e6
        + usage.system.seconds as f64
        + usage.system.microseconds as f64 / 1e6
}

struct Arguments {
    track: PathBuf,
    targets: Vec<u64>,
    rate_mbit: Option<f64>,
    settle: Duration,
}

fn parse() -> Result<Arguments, Box<dyn Error>> {
    let mut track = None;
    let mut targets = vec![5_000, 30_000, 120_000];
    let mut rate_mbit = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--targets-ms" => {
                targets = arguments
                    .next()
                    .ok_or("--targets-ms needs a list")?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()?;
            }
            "--rate-mbit" => {
                rate_mbit = Some(
                    arguments
                        .next()
                        .ok_or("--rate-mbit needs a value")?
                        .parse()?,
                )
            }
            path => track = Some(PathBuf::from(path)),
        }
    }

    Ok(Arguments {
        track: track
            .ok_or("usage: fallback_seek_benchmark <track> [--targets-ms a,b,c] [--rate-mbit n]")?,
        targets,
        rate_mbit,
        settle: Duration::from_secs(2),
    })
}

#[derive(Default)]
struct Window {
    frames: u64,
    underruns: u64,
    worst_underrun_ms: f64,
    worst_gap_ms: f64,
}

fn window(state: &Mutex<Pacing>, since: &Pacing) -> Window {
    let state = state.lock().unwrap();

    Window {
        frames: state.frames - since.frames,
        underruns: state.underruns - since.underruns,
        worst_underrun_ms: state.worst_underrun_ms,
        worst_gap_ms: state.worst_gap_ms,
    }
}

fn snapshot(state: &Mutex<Pacing>) -> Pacing {
    let mut state = state.lock().unwrap();
    // Peaks are reported per measurement window.
    let copy = Pacing {
        frames: state.frames,
        underruns: state.underruns,
        ..Pacing::default()
    };
    state.worst_underrun_ms = 0.0;
    state.worst_gap_ms = 0.0;

    copy
}

/// One seek: how long it took and what playback looked like meanwhile.
struct Measured {
    latency: Duration,
    cpu: f64,
    window: Window,
    applied: Result<(), String>,
}

fn measure(session: &PlaybackSession, pacing: &Mutex<Pacing>, target: u64) -> Measured {
    let before = snapshot(pacing);
    let cpu_before = cpu_seconds();
    let started = Instant::now();
    let applied = session
        .seek(Duration::from_millis(target))
        .map_err(|e| e.to_string());
    let latency = started.elapsed();
    let cpu = cpu_seconds() - cpu_before;
    // Let the swap's effects (and any late frame it caused) show up in the window.
    thread::sleep(Duration::from_millis(300));

    Measured {
        latency,
        cpu,
        window: window(pacing, &before),
        applied,
    }
}

fn run(
    label: &str,
    source: InputSource,
    fallback: bool,
    arguments: &Arguments,
    origin: Option<&Origin>,
) -> Result<(), Box<dyn Error>> {
    for target in &arguments.targets {
        let session = PlaybackSession::new();
        session.configure_fallback_seeking(fallback);
        let (output, pacing) = PacedOutput::new();
        session.play(source.clone(), output)?;
        thread::sleep(arguments.settle);

        // Seeking backwards from a point at least `settle` into the track.
        let requests_before = origin.map_or(0, |o| o.connections.load(Ordering::SeqCst));
        let bytes_before = origin.map_or(0, |o| o.bytes.load(Ordering::Relaxed));
        let measured = measure(&session, &pacing, *target);
        let position = session
            .position()
            .media
            .map(|media| media.position.as_millis() as u64);
        println!(
            "{}",
            serde_json::json!({
                "source": label,
                "fallback": fallback,
                "targetMs": target,
                "latencyMs": measured.latency.as_secs_f64() * 1000.0,
                "cpuSeconds": measured.cpu,
                "applied": measured.applied.is_ok(),
                "error": measured.applied.err(),
                "positionAfterMs": position,
                "framesDuringSeek": measured.window.frames,
                "underruns": measured.window.underruns,
                "worstUnderrunMs": measured.window.worst_underrun_ms,
                "worstGapMs": measured.window.worst_gap_ms,
                "extraConnections": origin.map_or(0, |o| o.connections.load(Ordering::SeqCst)) - requests_before,
                "extraBytesServed": origin.map_or(0, |o| o.bytes.load(Ordering::Relaxed)) - bytes_before,
            })
        );
        session.stop()?;
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = parse()?;
    let body = Arc::new(std::fs::read(&arguments.track)?);
    let rate = arguments
        .rate_mbit
        .map(|mbit| (mbit * 1_000_000.0 / 8.0) as u64);
    let origin = serve(Arc::clone(&body), rate);

    // Without the option the same seek on the same origin takes FFmpeg's own path.
    run(
        "http-rangeless",
        InputSource::http_url(&origin.url),
        false,
        &arguments,
        Some(&origin),
    )?;
    run(
        "http-rangeless",
        InputSource::http_url(&origin.url),
        true,
        &arguments,
        Some(&origin),
    )?;
    // A seekable local file is the floor: a demuxer seek with no second input.
    run(
        "local-file",
        InputSource::file(&arguments.track),
        false,
        &arguments,
        None,
    )?;

    Ok(())
}
