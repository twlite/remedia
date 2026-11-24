//! Isolate real sender/router admission and acknowledgements with an async controlled consumer.
//! This measures the queue boundary, not network/RTP/DAVE cost or live Discord quality.
use super::*;
use remedia_core::{
    InputSource, OpusEncoderOutput, PlaybackScheduler, PlaybackSession, PlaybackState,
};
use std::time::Instant as Clock;

#[derive(Default)]
struct Delivery {
    samples: u64,
    packets: u64,
    checksum: u64,
    first_ms: Option<f64>,
    worst_lateness_ms: f64,
    worst_gap_ms: f64,
    last: Option<(Clock, u32)>,
}

fn consumer(
    gate: watch::Receiver<bool>,
    epoch: Clock,
    paced: bool,
    stagger_ms: u64,
    stalled: bool,
) -> (DiscordVoiceConnection, Arc<Mutex<Delivery>>) {
    let (packet_tx, packet_rx) = flume::bounded::<QueuedPacket>(QUEUE_CAPACITY);
    let (control_tx, control_rx) = flume::bounded(16);
    let (generation_tx, _) = watch::channel(0);
    let (state, _) = watch::channel(ConnectionState::Connected);
    let cancellation = TokioCancellationToken::new();
    let readiness = Arc::new(futures_util::task::AtomicWaker::new());
    let delivery = Arc::new(Mutex::new(Delivery::default()));
    let (stats, notify, cancel) = (delivery.clone(), readiness.clone(), cancellation.clone());
    let worker = tokio::spawn(async move {
        let mut gate = gate;
        gate.wait_for(|open| *open).await.unwrap();
        time::sleep(Duration::from_millis(stagger_ms)).await;
        let mut processed = 0;
        let mut barriers = Vec::<(u64, flume::Sender<Result<(), VoiceError>>)>::new();
        let mut deadline = Instant::now();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                packet = packet_rx.recv_async() => {
                    let Ok(queued) = packet else { break; };
                    notify.wake();
                    if paced { time::sleep_until(deadline).await; }
                    let packet = queued.packet;
                    let stall_now = {
                        let mut stats = lock(&stats);
                        let now = Clock::now();
                        stats.first_ms.get_or_insert_with(|| now.duration_since(epoch).as_secs_f64() * 1000.0);
                        if paced { stats.worst_lateness_ms = stats.worst_lateness_ms.max(Instant::now().saturating_duration_since(deadline).as_secs_f64() * 1000.0); }
                        if let Some((previous, duration)) = stats.last { stats.worst_gap_ms = stats.worst_gap_ms.max(now.duration_since(previous).as_secs_f64() * 1000.0 - f64::from(duration) / 48.0); }
                        stats.last = Some((now, packet.samples_per_channel));
                        assert_eq!(packet.start_sample, stats.samples);
                        if stats.packets == 0 { stats.checksum = 0xcbf29ce484222325; }
                        for byte in packet.bytes.iter().copied().chain(packet.start_sample.to_le_bytes()).chain(packet.media_samples_per_channel.to_le_bytes()).chain(packet.samples_per_channel.to_le_bytes()) {
                            stats.checksum = (stats.checksum ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                        }
                        stats.samples += u64::from(packet.media_samples_per_channel);
                        stats.packets += 1;
                        queued.played_end_sample.store(stats.samples, Ordering::Release);
                        stalled && stats.packets == 25
                    };
                    processed = queued.id;
                    deadline += Duration::from_secs_f64(f64::from(packet.samples_per_channel) / 48_000.0);
                    if stall_now { time::sleep(Duration::from_millis(100)).await; }
                }
                control = control_rx.recv_async() => match control {
                    Ok(Control::Pause { target_id, ack } | Control::Finish { target_id, ack }) => barriers.push((target_id, ack)),
                    Ok(Control::Resume { ack } | Control::Flush { ack, .. }) => { let _ = ack.send(Ok(())); }
                    Ok(Control::Cancel { .. }) => {},
                    Err(_) => break,
                }
            }
            let mut done = Vec::new();
            barriers.retain(|(target, ack)| {
                if processed >= *target {
                    done.push(ack.clone());
                    false
                } else {
                    true
                }
            });
            // Production adds a five-packet silence tail before acknowledging a barrier.
            if !done.is_empty() && paced {
                time::sleep(Duration::from_millis(100)).await;
            }
            for ack in done {
                let _ = ack.send(Ok(()));
            }
        }
        notify.wake();
    });
    (
        DiscordVoiceConnection {
            inner: Arc::new(ConnectionInner {
                voice_connection_id: 1,
                guild_id: 1,
                observability: ObservabilityHandle::default(),
                state,
                failure: Arc::new(Mutex::new(None)),
                packet_tx,
                readiness,
                control_tx,
                generation: AtomicU64::new(0),
                generation_update: Mutex::new(()),
                generation_tx,
                cancelled_generation: Arc::new(AtomicU64::new(u64::MAX)),
                accepted_packet_id: AtomicU64::new(0),
                next_packet_id: AtomicU64::new(0),
                cancellation,
                worker: Mutex::new(Some(worker)),
            }),
        },
        delivery,
    )
}

#[test]
#[ignore = "release queue-boundary benchmark; configure HYBRID_* and run explicitly"]
fn scheduled_transport_queue_benchmark() {
    let number = |name, default: &str| {
        std::env::var(name)
            .unwrap_or(default.into())
            .parse::<usize>()
            .unwrap()
    };
    let fixture = std::env::var("HYBRID_FIXTURE").expect("HYBRID_FIXTURE");
    let (count, workers, budget) = (
        number("HYBRID_SESSIONS", "64"),
        number("HYBRID_WORKERS", "4"),
        number("HYBRID_BUDGET", "8"),
    );
    let flag = |name| std::env::var(name).as_deref() == Ok("1");
    let (paced, stagger, stalls) = (
        flag("HYBRID_PACED"),
        flag("HYBRID_STAGGER"),
        flag("HYBRID_STALLS"),
    );
    let encoded = std::fs::read(&fixture).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let before = remedia_resources::collect();
        let epoch = Clock::now();
        let sampling_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0;
        let pool = (workers != 0).then(|| PlaybackScheduler::with_processing_budget(workers, count + 1, budget).unwrap());
        let (gate, closed) = watch::channel(false);
        let (mut sessions, mut connections, mut deliveries, mut startups) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for index in 0..count {
            let (connection, delivery) = consumer(closed.clone(), epoch, paced, if stagger { (index % 10) as u64 * 10 } else { 0 }, stalls && index % 8 == 0);
            let router = crate::DiscordOpusSenderRouter::new();
            router.attach(connection.opus_sender());
            let session = pool.as_ref().map_or_else(PlaybackSession::new, |pool| PlaybackSession::with_scheduler(pool.clone()));
            let start = Clock::now();
            session.play(InputSource::bytes(encoded.clone()), OpusEncoderOutput::new(router.sink()).unwrap()).unwrap();
            startups.push(start.elapsed().as_secs_f64() * 1000.0);
            sessions.push(session); connections.push(connection); deliveries.push(delivery);
        }
        if let Some(pool) = &pool {
            let limit = Clock::now() + Duration::from_secs(10);
            while pool.diagnostics().admitted < count { assert!(Clock::now() < limit); std::thread::sleep(Duration::from_millis(1)); }
            let d = pool.diagnostics();
            assert_eq!(d.scheduled_sessions, count);
            assert_eq!(d.unsupported_fallbacks + d.capacity_fallbacks + d.control_fallbacks, 0);
        }
        let startup_seconds = epoch.elapsed().as_secs_f64();
        let active = remedia_resources::collect();
        gate.send_replace(true);
        for session in &sessions { assert_eq!(session.wait(), PlaybackState::Completed, "{:?}", session.last_error()); session.stop().unwrap(); }
        let after = remedia_resources::collect();
        let samples: Vec<_> = deliveries.iter().map(|s| lock(s).samples).collect();
        let checksums: Vec<_> = deliveries.iter().map(|s| lock(s).checksum).collect();
        let packets: Vec<_> = deliveries.iter().map(|s| lock(s).packets).collect();
        assert!(samples.iter().all(|s| *s > 0 && *s == samples[0]));
        assert!(checksums.iter().all(|s| *s == checksums[0]));
        let diagnostics = pool.as_ref().map(|pool| { let d = pool.diagnostics(); serde_json::json!({"admitted":d.admitted,"controlFallbacks":d.control_fallbacks,"unsupportedFallbacks":d.unsupported_fallbacks,"capacityFallbacks":d.capacity_fallbacks,"queuePeak":d.queue_peak,"queueContentions":d.queue_contentions,"queueWaitNs":d.queue_wait_ns,"turns":d.turns,"actions":d.actions,"workerBusyNs":d.worker_busy_ns,"startupWorkers":d.startup_workers,"controlWorkers":d.control_workers,"ioWorkers":d.io_workers}) });
        println!("HYBRID_RESULT={}", serde_json::json!({"scope":"controlled-discord-queue","sessions":count,"workers":workers,"budget":budget,"paced":paced,"stagger":stagger,"stalls":stalls,"fixture":fixture,"samplingEpochUnixMs":sampling_epoch,"startupSeconds":startup_seconds,"wallSeconds":epoch.elapsed().as_secs_f64(),"samples":samples,"checksums":checksums,"packets":packets,"audioSeconds":samples.iter().sum::<u64>() as f64/48000.0,"startupLatenciesMs":startups,"firstPacketMs":deliveries.iter().map(|s|lock(s).first_ms).collect::<Vec<_>>(),"worstLatenessMs":deliveries.iter().map(|s|lock(s).worst_lateness_ms).collect::<Vec<_>>(),"worstGapExcessMs":deliveries.iter().map(|s|lock(s).worst_gap_ms).collect::<Vec<_>>(),"diagnostics":diagnostics,"before":before,"active":active,"after":after}));
        for connection in connections { connection.disconnect().await.unwrap(); }
    });
}
