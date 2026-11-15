//! Controlled synchronous sink benchmark; no Discord or external services.
//!
//! Usage: `concurrent_playback_benchmark FILE SESSIONS WORKLOAD PACING [COMPLEXITY] [OPTIONS]`
//! where `WORKLOAD` is `passthrough`, `volume`, `filter`, `crossfade`, `seek`, `transition` or
//! `stream`, and `OPTIONS` may include `controls`, `alloc` (count heap activity) and
//! `seconds=N` (stop every session after N wall seconds). See `benchmarks/README.md`.
use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use remedia_core::{
    CancellationToken, CrossfadeEvent, EncodedOpusPacket, InputSource, OpusEncoderConfig,
    OpusEncoderOutput, OpusPacketSink, OutputError, PlaybackSession, PlaybackState, StageIdentity,
};

/// Optional heap accounting. It is disabled unless `alloc` is passed so CPU and RSS runs pay only
/// one relaxed load per allocation.
struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator under the caller's contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() && COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            LIVE_BYTES.fetch_sub(
                layout.size().min(LIVE_BYTES.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
        // SAFETY: the pointer and layout come from a matching `alloc` call.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator under the caller's contract.
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() && COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            if new_size >= layout.size() {
                let live = LIVE_BYTES.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                    + (new_size - layout.size());
                PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        resized
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Default)]
struct Counts {
    epoch: Option<Instant>,
    samples: u64,
    clock_samples: u64,
    paused: Option<Instant>,
    packets: u64,
    capacity: u64,
    startup_us: u128,
    delays_us: Vec<u128>,
}

struct Sink {
    counts: Arc<Mutex<Counts>>,
    started: Instant,
    paced: bool,
}

impl OpusPacketSink for Sink {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        token: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut counts = self.counts.lock().unwrap();
        if counts.epoch.is_none() {
            counts.epoch = Some(Instant::now());
        }
        if counts.packets == 0 {
            counts.startup_us = self.started.elapsed().as_micros();
        }
        let deadline =
            counts.epoch.unwrap() + Duration::from_secs_f64(counts.clock_samples as f64 / 48_000.0);
        if self.paced {
            while Instant::now() < deadline {
                if token.is_cancelled() {
                    return Err(OutputError::Cancelled);
                }
                // Sleep coarsely until shortly before the deadline, then in short slices. This
                // keeps cancellation responsive and delay precise without spending most of the
                // measured CPU in 1 ms timer wakeups that a real packet queue does not have.
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(if remaining > Duration::from_millis(3) {
                    (remaining - Duration::from_millis(2)).min(Duration::from_millis(10))
                } else {
                    remaining.min(Duration::from_millis(1))
                });
            }
            counts.delays_us.push(
                Instant::now()
                    .saturating_duration_since(deadline)
                    .as_micros(),
            );
        }
        counts.samples += u64::from(packet.media_samples_per_channel);
        counts.clock_samples += u64::from(packet.media_samples_per_channel);
        counts.packets += 1;
        counts.capacity += packet.bytes.capacity() as u64;
        Ok(())
    }

    fn flush(&self, _: Duration, _: &CancellationToken) -> Result<(), OutputError> {
        let mut counts = self.counts.lock().unwrap();
        counts.epoch = Some(Instant::now());
        counts.clock_samples = 0;
        counts.paused = counts.paused.map(|_| Instant::now());
        Ok(())
    }

    fn pause(&self, _: &CancellationToken) -> Result<(), OutputError> {
        self.counts.lock().unwrap().paused = Some(Instant::now());
        Ok(())
    }

    fn resume(&self, _: &CancellationToken) -> Result<(), OutputError> {
        let mut counts = self.counts.lock().unwrap();
        if let Some(paused) = counts.paused.take() {
            counts.epoch = counts.epoch.map(|epoch| epoch + paused.elapsed());
        }
        Ok(())
    }
    fn finish(&self, _: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }
    fn cancel(&self) {}
}

fn percentile(values: &mut [u128], percent: usize) -> u128 {
    values.sort_unstable();
    values
        .get(values.len().saturating_sub(1) * percent / 100)
        .copied()
        .unwrap_or(0)
}

struct Options {
    controls: bool,
    alloc: bool,
    seconds: Option<f64>,
}

/// Run `action` over `items` with a few threads so one slow command cannot delay the others.
fn parallel_each<T: Sync>(items: &[T], action: impl Fn(&T) + Sync) {
    let threads = 8.min(items.len().max(1));
    std::thread::scope(|scope| {
        for lane in 0..threads {
            let action = &action;
            scope.spawn(move || {
                for item in items.iter().skip(lane).step_by(threads) {
                    action(item);
                }
            });
        }
    });
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 5 {
        return Err("usage: concurrent_playback_benchmark FILE SESSIONS passthrough|volume|filter|crossfade|seek|transition|stream paced|unpaced [COMPLEXITY] [controls] [alloc] [seconds=N]".into());
    }
    let count: usize = args[2].parse()?;
    if count == 0 {
        return Err("sessions must be positive".into());
    }
    if !matches!(args[4].as_str(), "paced" | "unpaced") {
        return Err("pacing must be paced or unpaced".into());
    }
    let workload = args[3].as_str();
    let mut options = Options {
        controls: false,
        alloc: false,
        seconds: None,
    };
    let mut complexity = 9;
    for extra in &args[5..] {
        match extra.as_str() {
            "controls" => options.controls = true,
            "alloc" => options.alloc = true,
            other if other.starts_with("seconds=") => {
                options.seconds = Some(other["seconds=".len()..].parse()?)
            }
            other => complexity = other.parse()?,
        }
    }
    if matches!(workload, "seek" | "transition")
        && (args[4] != "paced" || options.seconds.is_none())
    {
        return Err(format!("{workload} requires paced pacing and seconds=N").into());
    }
    let config = OpusEncoderConfig {
        complexity,
        ..Default::default()
    };
    let (monitor_stop, monitor_rx) = std::sync::mpsc::channel();
    let monitor = std::thread::spawn(move || {
        let mut samples: Vec<remedia_resources::ProcessSnapshot> = Vec::new();
        let mut previous_at = Instant::now();
        loop {
            let mut sample = remedia_resources::collect();
            let now = Instant::now();
            if let Some(previous) = samples.last() {
                let interval = now.duration_since(previous_at).as_secs_f64() * 1000.0;
                sample.cpu_sample_interval_ms = Some(interval);
                sample.cpu.process_percent = previous
                    .cpu
                    .user_time_ms
                    .zip(previous.cpu.system_time_ms)
                    .zip(sample.cpu.user_time_ms.zip(sample.cpu.system_time_ms))
                    .map(|((a, b), (c, d))| 100.0 * (c + d - a - b).max(0.0) / interval);
            }
            previous_at = now;
            samples.push(sample);
            if !matches!(
                monitor_rx.recv_timeout(Duration::from_secs(1)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                break;
            }
        }
        samples
    });
    let mut commands = Vec::new();
    if options.alloc {
        COUNTING.store(true, Ordering::SeqCst);
    }
    let started = Instant::now();
    let mut sessions = Vec::new();
    let mut counters = Vec::new();
    // One shared feeder drives every stream session without per-session threads. It never
    // blocks: a chunk is written only when the bounded queue has room for it. It starts before
    // any session because `play` waits for the first bytes of a stream.
    let feeders: Arc<Mutex<Vec<(remedia_core::ByteStreamWriter, usize)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let feeding = Arc::new(AtomicBool::new(workload == "stream"));
    let feeder_thread = (workload == "stream").then(|| {
        let bytes = std::fs::read(&args[1]).expect("readable fixture");
        let feeders = Arc::clone(&feeders);
        let feeding = Arc::clone(&feeding);
        std::thread::spawn(move || {
            while feeding.load(Ordering::Relaxed) {
                let mut progressed = false;
                for (writer, offset) in feeders.lock().unwrap().iter_mut() {
                    if *offset > bytes.len() {
                        continue;
                    }
                    if *offset == bytes.len() {
                        let _ = writer.finish();
                        *offset += 1;
                        continue;
                    }
                    let end = (*offset + 16_384).min(bytes.len());
                    if writer.pending_bytes() + (end - *offset) <= writer.capacity_bytes() {
                        if writer.write_owned(bytes[*offset..end].to_vec()).is_err() {
                            *offset = bytes.len() + 1;
                            continue;
                        }
                        *offset = end;
                        progressed = true;
                    }
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        })
    });
    for _ in 0..count {
        let session = PlaybackSession::new();
        match workload {
            "passthrough" | "seek" | "stream" => {}
            "crossfade" => session.configure_crossfade(Some(
                remedia_core::CrossfadeConfig::new(1000, 5000).unwrap(),
            ))?,
            "transition" => session.configure_crossfade(Some(
                remedia_core::CrossfadeConfig::new(1000, 5000).unwrap(),
            ))?,
            "volume" => session.set_volume(0.999)?,
            "filter" => session.set_filter_graph(Some("tremolo=f=5:d=0.2"))?,
            _ => return Err("unknown workload".into()),
        }
        let counts = Arc::new(Mutex::new(Counts::default()));
        let sink = Sink {
            counts: Arc::clone(&counts),
            started: Instant::now(),
            paced: args[4] == "paced",
        };
        let input = if workload == "stream" {
            // A plugin-style producer: a bounded byte stream fed in network-sized chunks.
            let (input, writer) = InputSource::byte_stream(262_144)?;
            feeders.lock().unwrap().push((writer, 0_usize));
            input
        } else {
            InputSource::file(&args[1])
        };
        session.play(input, OpusEncoderOutput::new_with_config(sink, config)?)?;
        if options.controls {
            let command = Instant::now();
            session.pause()?;
            commands.push(command.elapsed().as_micros());
        }
        sessions.push(session);
        counters.push(counts);
    }
    if options.controls {
        for session in &sessions {
            let command = Instant::now();
            session.seek(Duration::from_millis(100))?;
            commands.push(command.elapsed().as_micros());
            let command = Instant::now();
            session.resume()?;
            commands.push(command.elapsed().as_micros());
        }
    }

    // Controller: sample modes, issue seeks/staging, and enforce an optional deadline.
    let duration_s = sessions
        .first()
        .and_then(|session| session.duration())
        .map_or(60.0, |duration| duration.as_secs_f64());
    let mut mode_samples = [0_u64; 2];
    let mut stage_serial = vec![0_u64; count];
    let mut next_seek: Vec<Instant> = (0..count)
        .map(|index| started + Duration::from_millis(2_000 + (index as u64 % 30) * 100))
        .collect();
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut seek_latencies = Vec::new();
    let mut transitions = 0_u64;
    let deadline = options.seconds.map(Duration::from_secs_f64);
    let supervised = deadline.is_some() || matches!(workload, "seek" | "transition");
    if supervised {
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let now = Instant::now();
            if deadline.is_some_and(|limit| started.elapsed() >= limit)
                || sessions.iter().all(|session| {
                    !matches!(
                        session.state(),
                        PlaybackState::Playing | PlaybackState::Starting
                    )
                })
            {
                break;
            }
            for session in &sessions {
                match session.processing_mode() {
                    Some(remedia_core::PlaybackProcessingMode::Passthrough) => mode_samples[0] += 1,
                    Some(_) => mode_samples[1] += 1,
                    None => {}
                }
            }
            if workload == "seek" {
                let due: Vec<usize> = (0..count)
                    .filter(|index| next_seek[*index] <= now)
                    .collect();
                let mut targets = Vec::new();
                for index in due {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    let fraction = 0.1 + 0.7 * ((seed >> 40) as f64 / (1_u64 << 24) as f64);
                    targets.push((index, Duration::from_secs_f64(duration_s * fraction)));
                    next_seek[index] = now + Duration::from_secs(3);
                }
                let latencies = Mutex::new(Vec::new());
                parallel_each(&targets, |(index, position)| {
                    let command = Instant::now();
                    if sessions[*index].seek(*position).is_ok() {
                        latencies
                            .lock()
                            .unwrap()
                            .push(command.elapsed().as_micros());
                    }
                });
                seek_latencies.extend(latencies.into_inner().unwrap());
            }
            if workload == "transition" {
                for (index, session) in sessions.iter().enumerate() {
                    for event in session.take_crossfade_events() {
                        match event {
                            CrossfadeEvent::PrepareRequested { generation, .. } => {
                                stage_serial[index] += 1;
                                let _ = session.stage_next(
                                    InputSource::file(&args[1]),
                                    StageIdentity {
                                        expected_generation: generation,
                                        entry_id: format!("entry-{}", stage_serial[index]),
                                        queue_revision: 1,
                                        source_revision: 1,
                                    },
                                );
                            }
                            CrossfadeEvent::TransitionCompleted { .. } => transitions += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        feeding.store(false, Ordering::Relaxed);
        for session in &sessions {
            session.stop()?;
        }
    } else {
        for session in &sessions {
            if session.wait() != PlaybackState::Completed {
                return Err(format!("playback failed: {:?}", session.last_error()).into());
            }
            session.stop()?;
        }
    }
    commands.extend(seek_latencies);
    feeding.store(false, Ordering::Relaxed);
    if let Some(feeder) = feeder_thread {
        let _ = feeder.join();
    }
    let allocation_snapshot = options.alloc.then(|| {
        COUNTING.store(false, Ordering::SeqCst);
        serde_json::json!({
            "allocations": ALLOCATIONS.load(Ordering::SeqCst),
            "allocated_bytes": ALLOCATED_BYTES.load(Ordering::SeqCst),
            "peak_live_bytes": PEAK_LIVE_BYTES.load(Ordering::SeqCst),
        })
    });
    let mut delays = Vec::new();
    let mut startup = Vec::new();
    let (mut samples, mut packets, mut capacity) = (0, 0, 0);
    for counts in counters {
        let mut counts = counts.lock().unwrap();
        samples += counts.samples;
        packets += counts.packets;
        capacity += counts.capacity;
        startup.push(counts.startup_us);
        delays.append(&mut counts.delays_us);
    }
    let _ = monitor_stop.send(());
    let resources = monitor.join().expect("resource collector");
    let observed_modes = mode_samples[0] + mode_samples[1];
    println!(
        "{}",
        serde_json::json!({
            "wall_s": started.elapsed().as_secs_f64(),
            "audio_s": samples as f64 / 48_000.0,
            "packets": packets,
            "packet_capacity_bytes": capacity,
            "startup_p99_us": percentile(&mut startup, 99),
            "sink_delay_p95_us": percentile(&mut delays, 95),
            "sink_delay_p99_us": percentile(&mut delays, 99),
            "command_p95_us": percentile(&mut commands, 95),
            "command_p99_us": percentile(&mut commands, 99),
            "passthrough_fraction": (observed_modes > 0).then(|| mode_samples[0] as f64 / observed_modes as f64),
            "transitions": transitions,
            "heap": allocation_snapshot,
            "resource_samples": resources,
        })
    );
    Ok(())
}
