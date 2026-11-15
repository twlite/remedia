//! Measures the resource cost of the plugin media transfer's one-OS-thread-per-stream design.
//!
//! The shapes mirror `plugin_host::media::open_media`: each stream owns a bounded
//! `ByteStreamWriter` and a dedicated `std::thread` that runs `Handle::block_on` over a pump
//! loop. Usage (one fresh process per measurement, so resident memory is attributable):
//!
//! ```text
//! cargo run --release -p remedia-server --example plugin_media_threads -- <mode> <streams> <buffer-bytes>
//! ```
//!
//! Modes:
//! - `blocked`: every stream's buffer is full, so its thread is parked in `write_owned`, which is
//!   what a paused or slow player causes.
//! - `idle`: every thread waits for upstream data, polling the writer every 250 ms like the real
//!   pump's `select!`.
//! - `tasks`: the idle poll loop as plain Tokio tasks with no dedicated thread, an upper bound
//!   for what an async design could save when streams wait on the network.
//!
//! One JSON line is printed with resident memory, OS thread count and CPU time over a window.

use std::process::Command;
use std::time::{Duration, Instant};

use remedia_core::{ByteStreamWriter, InputSource};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CHUNK: usize = 16 * 1024;
const WINDOW: Duration = Duration::from_secs(10);

fn ps(field: &str) -> String {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &pid])
        .output()
        .expect("run ps");

    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn thread_count() -> usize {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-M", "-p", &pid])
        .output()
        .expect("run ps");

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}

/// `ps` CPU time (`[[dd-]hh:]mm:ss.cc` on macOS, `[[dd-]hh:]mm:ss` on Linux) in seconds.
fn cpu_seconds() -> f64 {
    let raw = ps("cputime");
    let (days, rest) = raw.split_once('-').map_or((0.0, raw.as_str()), |(d, r)| {
        (d.parse::<f64>().unwrap_or(0.0), r)
    });

    days * 86_400.0
        + rest.split(':').fold(0.0, |total, part| {
            total * 60.0 + part.parse::<f64>().unwrap_or(0.0)
        })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode: blocked | idle | tasks");
    let streams: usize = args.next().expect("streams").parse().expect("stream count");
    let buffer_bytes: usize = args
        .next()
        .expect("buffer bytes")
        .parse()
        .expect("buffer size");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let handle = runtime.handle().clone();
    let baseline_rss: u64 = ps("rss").parse().unwrap();
    let baseline_threads = thread_count();

    // Keep sources alive: dropping one closes the stream and ends its pump.
    let mut sources: Vec<InputSource> = Vec::new();
    let mut joins = Vec::new();

    for _ in 0..streams {
        let chunk = CHUNK.min(buffer_bytes);
        let (source, writer): (InputSource, ByteStreamWriter) =
            InputSource::byte_stream(buffer_bytes).expect("byte stream");
        sources.push(source);

        match mode.as_str() {
            "blocked" => {
                let handle = handle.clone();
                joins.push(
                    std::thread::Builder::new()
                        .name("remedia-plugin-media".into())
                        .spawn(move || {
                            handle.block_on(async move {
                                while writer.write_owned(vec![0_u8; chunk]).is_ok() {}
                            });
                        })
                        .expect("spawn pump thread"),
                );
            }
            "idle" => {
                let handle = handle.clone();
                joins.push(
                    std::thread::Builder::new()
                        .name("remedia-plugin-media".into())
                        .spawn(move || {
                            handle.block_on(async move {
                                while !writer.is_closed() {
                                    tokio::time::sleep(POLL_INTERVAL).await;
                                }
                            });
                        })
                        .expect("spawn pump thread"),
                );
            }
            "tasks" => {
                handle.spawn(async move {
                    while !writer.is_closed() {
                        tokio::time::sleep(POLL_INTERVAL).await;
                    }
                });
            }
            other => panic!("unknown mode {other}"),
        }
    }

    // Let every thread reach its steady state (buffers full / first poll armed).
    std::thread::sleep(Duration::from_secs(2));
    let rss: u64 = ps("rss").parse().unwrap();
    let threads = thread_count();
    let cpu_start = cpu_seconds();
    let started = Instant::now();
    std::thread::sleep(WINDOW);
    let cpu = cpu_seconds() - cpu_start;
    let wall = started.elapsed().as_secs_f64();

    println!(
        "{{\"mode\":\"{mode}\",\"streams\":{streams},\"buffer_bytes\":{buffer_bytes},\
         \"rss_kib\":{},\"rss_delta_kib\":{},\"threads\":{},\"threads_delta\":{},\
         \"rss_per_stream_kib\":{:.1},\"cpu_percent\":{:.2}}}",
        rss,
        rss.saturating_sub(baseline_rss),
        threads,
        threads.saturating_sub(baseline_threads),
        rss.saturating_sub(baseline_rss) as f64 / streams.max(1) as f64,
        cpu / wall * 100.0,
    );

    drop(sources);
    for join in joins {
        let _ = join.join();
    }
}
