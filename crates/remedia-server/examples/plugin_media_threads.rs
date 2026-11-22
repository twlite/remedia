//! Compare plugin-media transfer thread strategies with bounded stream queues.
//!
//! This is a synthetic resource harness, not an HTTP or decoder benchmark. Each run uses fresh
//! byte streams and records process RSS, footprint, thread count, queue occupancy, and cleanup.
//!
//! Usage: cargo run --release -p remedia-server --example plugin_media_threads -- \
//!   <dedicated-blocked|pool-blocked|dedicated-idle|async-idle> <streams> <buffer-bytes>
//!
//! dedicated-blocked mirrors the production one-thread-per-stream pump while each decoder is
//! stalled. pool-blocked parks Tokio blocking-pool workers on the same synchronous writer and
//! records whether an unrelated blocking job can start. dedicated-idle and async-idle compare
//! thread-per-stream and task-per-stream waits for upstream data. The task modes are resource
//! bounds only; this harness does not implement an async queue or exercise HTTP/FFmpeg.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use remedia_core::{ByteStreamWriter, InputSource};
use remedia_resources::collect;
use tokio::task::JoinHandle;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CHUNK_BYTES: usize = 16 * 1024;
const MAX_BLOCKING_THREADS: usize = 16;
const SETTLE: Duration = Duration::from_millis(250);
const SAMPLE_WINDOW: Duration = Duration::from_millis(500);
const STARVATION_PROBE: Duration = Duration::from_millis(100);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode");
    assert!(
        matches!(
            mode.as_str(),
            "dedicated-blocked" | "pool-blocked" | "dedicated-idle" | "async-idle"
        ),
        "unknown transfer mode"
    );

    let streams = args
        .next()
        .expect("stream count")
        .parse::<usize>()
        .expect("stream count must be a positive integer");
    let buffer_bytes = args
        .next()
        .expect("buffer size")
        .parse::<usize>()
        .expect("buffer size must be a positive integer");
    assert!(streams > 0 && buffer_bytes > 0);

    let initial = collect();
    let initial_threads = initial.cpu.thread_count.unwrap_or_default();
    let initial_rss = initial.memory.resident_bytes;
    let blocking_threads = streams.min(MAX_BLOCKING_THREADS);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(blocking_threads)
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let handle = runtime.handle().clone();

    runtime.block_on(async {
        tokio::task::yield_now().await;
    });

    let baseline = collect();
    let baseline_threads = baseline.cpu.thread_count.unwrap_or_default();
    let baseline_rss = baseline.memory.resident_bytes;
    let mut sources = Vec::with_capacity(streams);
    let mut writers = Vec::with_capacity(streams);
    let mut dedicated_threads = Vec::new();
    let mut task_joins: Vec<JoinHandle<()>> = Vec::new();
    let (ready_tx, ready_rx) = mpsc::channel();

    for _ in 0..streams {
        let (source, writer) =
            InputSource::byte_stream(buffer_bytes).expect("create bounded stream");
        sources.push(source);
        writers.push(writer.clone());

        match mode.as_str() {
            "dedicated-blocked" => {
                let handle = handle.clone();
                let ready = ready_tx.clone();
                dedicated_threads.push(
                    thread::Builder::new()
                        .name("remedia-plugin-media".into())
                        .spawn(move || {
                            handle.block_on(async move {
                                write_until_closed(writer, buffer_bytes, ready);
                            });
                        })
                        .expect("spawn dedicated transfer thread"),
                );
            }
            "pool-blocked" => {
                let ready = ready_tx.clone();
                task_joins.push(handle.spawn_blocking(move || {
                    write_until_closed(writer, buffer_bytes, ready);
                }));
            }
            "dedicated-idle" => {
                let handle = handle.clone();
                dedicated_threads.push(
                    thread::Builder::new()
                        .name("remedia-plugin-media".into())
                        .spawn(move || {
                            handle.block_on(wait_until_closed(writer));
                        })
                        .expect("spawn dedicated transfer thread"),
                );
            }
            "async-idle" => {
                task_joins.push(handle.spawn(wait_until_closed(writer)));
            }
            _ => unreachable!("mode was validated above"),
        }
    }

    let expected_full_queues = match mode.as_str() {
        "dedicated-blocked" => streams,
        "pool-blocked" => streams.min(blocking_threads),
        _ => 0,
    };

    for _ in 0..expected_full_queues {
        ready_rx
            .recv_timeout(CLEANUP_TIMEOUT)
            .expect("a producer did not fill its bounded queue");
    }

    let full_queues = writers
        .iter()
        .filter(|writer| writer.pending_bytes() == buffer_bytes)
        .count();
    assert_eq!(full_queues, expected_full_queues);

    thread::sleep(SETTLE);
    let (sentinel_tx, sentinel_rx) = tokio::sync::oneshot::channel();
    let sentinel = handle.spawn_blocking(move || {
        let _ = sentinel_tx.send(());
    });
    let sentinel_started = std::time::Instant::now();
    let sentinel_ready_before_release = runtime.block_on(async {
        tokio::time::timeout(STARVATION_PROBE, sentinel_rx)
            .await
            .is_ok()
    });
    let sentinel_wait_us = sentinel_started.elapsed().as_micros();

    thread::sleep(SAMPLE_WINDOW);
    let sampled = collect();
    let pending_bytes = writers
        .iter()
        .map(ByteStreamWriter::pending_bytes)
        .sum::<usize>();

    drop(sources);
    let pending_bytes_after_release = writers
        .iter()
        .map(ByteStreamWriter::pending_bytes)
        .sum::<usize>();
    let open_writers_after_release = writers.iter().filter(|writer| !writer.is_closed()).count();
    assert_eq!(pending_bytes_after_release, 0);
    assert_eq!(open_writers_after_release, 0);

    for join in dedicated_threads {
        join.join().expect("dedicated transfer thread panicked");
    }

    runtime.block_on(async {
        tokio::time::timeout(CLEANUP_TIMEOUT, async {
            for join in task_joins {
                join.await.expect("Tokio transfer task panicked");
            }

            sentinel.await.expect("blocking-pool sentinel panicked");
        })
        .await
        .expect("transfer tasks did not finish after their consumers closed");
    });

    drop(writers);
    drop(handle);
    drop(runtime);

    let cleanup_started = Instant::now();
    let cleaned = loop {
        let snapshot = collect();
        let threads = snapshot.cpu.thread_count.unwrap_or_default();
        if threads <= initial_threads || cleanup_started.elapsed() >= CLEANUP_TIMEOUT {
            break snapshot;
        }

        thread::sleep(CLEANUP_POLL_INTERVAL);
    };
    let output = serde_json::json!({
        "mode": mode,
        "streams": streams,
        "buffer_bytes": buffer_bytes,
        "blocking_pool_threads": blocking_threads,
        "full_queues": full_queues,
        "pending_bytes": pending_bytes,
        "sentinel_ready_before_release": sentinel_ready_before_release,
        "sentinel_wait_us": sentinel_wait_us,
        "rss_bytes": sampled.memory.resident_bytes,
        "rss_delta_bytes": delta(baseline_rss, sampled.memory.resident_bytes),
        "peak_rss_bytes": sampled.memory.peak_resident_bytes,
        "footprint_bytes": sampled.memory.footprint_bytes,
        "threads": sampled.cpu.thread_count,
        "threads_delta": sampled.cpu.thread_count.map(|count| count.saturating_sub(baseline_threads)),
        "pending_bytes_after_release": pending_bytes_after_release,
        "open_writers_after_release": open_writers_after_release,
        "cleanup_rss_bytes": cleaned.memory.resident_bytes,
        "cleanup_rss_delta_bytes": delta(initial_rss, cleaned.memory.resident_bytes),
        "cleanup_peak_rss_bytes": cleaned.memory.peak_resident_bytes,
        "cleanup_footprint_bytes": cleaned.memory.footprint_bytes,
        "cleanup_threads": cleaned.cpu.thread_count,
        "cleanup_threads_delta": cleaned.cpu.thread_count.map(|count| count.saturating_sub(initial_threads)),
        "cleanup_wait_us": cleanup_started.elapsed().as_micros(),
        "native_heap_measured": false,
    });
    println!("{output}");
}

fn delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    before
        .zip(after)
        .map(|(before, after)| after.saturating_sub(before))
}

fn write_until_closed(writer: ByteStreamWriter, capacity_bytes: usize, ready: mpsc::Sender<()>) {
    let chunk_bytes = CHUNK_BYTES.min(capacity_bytes);
    let mut reported_full = false;

    while writer.write_owned(vec![0; chunk_bytes]).is_ok() {
        if !reported_full && writer.pending_bytes() == capacity_bytes {
            let _ = ready.send(());
            reported_full = true;
        }
    }
}

async fn wait_until_closed(writer: ByteStreamWriter) {
    while !writer.is_closed() {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
