//! Release-only controlled comparison: FILE SESSIONS WORKERS FORMAT.
//! WORKERS=0 selects dedicated execution; FORMAT is null, s16le, f32le or opus.
// Shared benchmark-only start gate: all opened sessions stay live until startup ends.
use remedia_core::{CancellationToken, EncodedOpusOutput, MediaOutput, OutputError, PcmFrame};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Waker;
#[cfg(feature = "experimental-scheduler")]
use std::task::{Context, Poll};
use std::time::Duration;

#[derive(Default)]
struct StartGate {
    state: Mutex<(bool, Vec<Waker>)>,
    changed: Condvar,
}

impl StartGate {
    fn release(&self) {
        let wakers = {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            std::mem::take(&mut state.1)
        };
        self.changed.notify_all();
        for waker in wakers {
            waker.wake();
        }
    }
}

struct GatedOutput<O> {
    gate: Arc<StartGate>,
    control_wake: std::sync::atomic::AtomicBool,
    output: O,
}

impl<O: MediaOutput> MediaOutput for GatedOutput<O> {
    fn wait_ready(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        let mut state = self.gate.state.lock().unwrap();
        while !state.0
            && !self.control_wake.load(std::sync::atomic::Ordering::Acquire)
            && !cancellation.is_cancelled()
        {
            state = self.gate.changed.wait(state).unwrap();
        }
        let closed = !state.0;
        drop(state);
        if closed {
            self.control_wake
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        self.output.wait_ready(cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_nonblocking(&self) -> bool {
        self.output.scheduler_nonblocking()
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_activate(&self) {
        self.output.scheduler_activate();
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_control(
        &self,
        control: remedia_core::OutputControl,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        self.output
            .scheduler_poll_control(control, cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_ready(
        &self,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        if cancellation.is_cancelled() {
            return Poll::Ready(Err(OutputError::Cancelled));
        }
        let mut state = self.gate.state.lock().unwrap();
        if !state.0
            && !self
                .control_wake
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            if !state.1.iter().any(|w| w.will_wake(cx.waker())) {
                state.1.push(cx.waker().clone());
            }
            return Poll::Pending;
        }
        drop(state);
        self.output.scheduler_poll_ready(cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_deadline(
        &self,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<Option<std::time::Instant>, OutputError>> {
        if cancellation.is_cancelled() {
            return Poll::Ready(Err(OutputError::Cancelled));
        }
        let mut state = self.gate.state.lock().unwrap();
        if !state.0
            && !self
                .control_wake
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            if !state.1.iter().any(|w| w.will_wake(cx.waker())) {
                state.1.push(cx.waker().clone());
            }
            return Poll::Pending;
        }
        drop(state);
        self.output.scheduler_poll_deadline(cx, cancellation)
    }

    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.output.write_frame(frame, cancellation)
    }
    fn encoded_opus_output(&self) -> Option<&dyn EncodedOpusOutput> {
        self.output.encoded_opus_output()
    }
    fn wake(&self) {
        self.control_wake
            .store(true, std::sync::atomic::Ordering::Release);
        self.gate.changed.notify_all();
        self.output.wake();
    }
    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.output.flush(position, cancellation)
    }
    fn flush_samples(
        &self,
        position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.output.flush_samples(position, cancellation)
    }
    fn filter_transition(
        &self,
        position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.output.filter_transition(position, cancellation)
    }
    fn pause(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.output.pause(cancellation)
    }
    fn resume(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.output.resume(cancellation)
    }
    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.output.finish(cancellation)
    }
    fn played_position(&self) -> Option<Duration> {
        self.output.played_position()
    }
    fn fail(&self, message: String) {
        self.output.fail(message);
    }
    fn cancel(&self) {
        self.output.cancel();
        let wakers = std::mem::take(&mut self.gate.state.lock().unwrap().1);
        self.gate.changed.notify_all();
        for waker in wakers {
            waker.wake();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use remedia_core::*;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::{Duration, Instant};

    struct ThreadWake(std::thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let args: Vec<_> = std::env::args().collect();
    let count: usize = args.get(2).ok_or("missing session count")?.parse()?;
    let workers: usize = args.get(3).ok_or("missing worker count")?.parse()?;
    let budget: usize = args
        .get(5)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1);
    #[cfg(not(feature = "experimental-scheduler"))]
    if workers != 0 {
        return Err("this binary supports dedicated execution only".into());
    }
    let format = match args.get(4).map(String::as_str) {
        Some("null") => None,
        Some("s16le") => Some(StreamFormat::S16le),
        Some("f32le") => Some(StreamFormat::F32le),
        Some("opus") => Some(StreamFormat::Opus),
        _ => return Err("expected null|s16le|f32le|opus".into()),
    };
    let pacing = if args.get(7).map(String::as_str) == Some("realtime") {
        StreamPacing::Realtime
    } else {
        StreamPacing::Unpaced
    };
    let paths: Vec<_> = args.get(1).ok_or("missing file")?.split('|').collect();
    let file_input = args.get(6).map(String::as_str) == Some("file");
    let encoded = if file_input {
        Vec::new()
    } else {
        paths
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()?
    };
    let startup_stagger_ms = args
        .get(8)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    let mut startup_latencies_ms = Vec::with_capacity(count);
    let before = remedia_resources::collect();
    let sampling_epoch_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64()
        * 1000.0;
    let start = Instant::now();
    #[cfg(feature = "experimental-scheduler")]
    let pool = if workers == 0 {
        None
    } else {
        Some(PlaybackScheduler::with_processing_budget(
            workers,
            count.max(1),
            budget,
        )?)
    };
    let gate = Arc::new(StartGate::default());
    let mut sessions = Vec::new();
    let mut readers = Vec::new();
    let mut null_stats = Vec::new();
    for index in 0..count {
        #[cfg(not(feature = "experimental-scheduler"))]
        let session = PlaybackSession::new();
        #[cfg(feature = "experimental-scheduler")]
        let session = pool.as_ref().map_or_else(PlaybackSession::new, |pool| {
            PlaybackSession::with_scheduler(pool.clone())
        });
        // Independent input storage per session; all modes pay the same input ownership cost.
        let source = if file_input {
            InputSource::file(paths[index % paths.len()])
        } else {
            InputSource::bytes(encoded[index % paths.len()].clone())
        };
        let opened_at = Instant::now();
        if let Some(format) = format {
            let (output, reader) = StreamOutput::new(format, pacing, OpusEncoderConfig::default())?;
            session.play(
                source,
                GatedOutput {
                    gate: gate.clone(),
                    control_wake: std::sync::atomic::AtomicBool::new(false),
                    output,
                },
            )?;
            readers.push(Some(reader));
        } else {
            let output = NullOutput::new();
            null_stats.push(output.stats_handle());
            session.play(
                source,
                GatedOutput {
                    gate: gate.clone(),
                    control_wake: std::sync::atomic::AtomicBool::new(false),
                    output,
                },
            )?;
        }
        startup_latencies_ms.push(opened_at.elapsed().as_secs_f64() * 1000.0);
        sessions.push(session);
        if startup_stagger_ms != 0 {
            std::thread::sleep(Duration::from_millis(startup_stagger_ms));
        }
    }
    #[cfg(feature = "experimental-scheduler")]
    if let Some(pool) = &pool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while {
            let d = pool.diagnostics();
            d.admitted + d.unsupported_fallbacks + d.capacity_fallbacks < count
        } {
            if Instant::now() >= deadline {
                return Err("scheduler admission did not finish".into());
            }
            std::thread::yield_now();
        }
    }
    let control_rounds = args
        .get(9)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    if control_rounds != 0 {
        gate.release();
    }
    let mut control_latencies: std::collections::BTreeMap<&str, Vec<f64>> =
        std::collections::BTreeMap::new();
    for session in &sessions {
        if control_rounds == 0 {
            break;
        }
        let at = Instant::now();
        session.pause()?;
        control_latencies
            .entry("pause")
            .or_default()
            .push(at.elapsed().as_secs_f64() * 1000.0);
        for round in 0..control_rounds {
            let at = Instant::now();
            session.seek(Duration::from_millis(100 + (round % 4) as u64 * 10))?;
            control_latencies
                .entry("seek")
                .or_default()
                .push(at.elapsed().as_secs_f64() * 1000.0);
            let at = Instant::now();
            session.set_filter_graph(if round % 2 == 0 {
                Some("volume=0.75")
            } else {
                None
            })?;
            control_latencies
                .entry("filter")
                .or_default()
                .push(at.elapsed().as_secs_f64() * 1000.0);
        }
        let at = Instant::now();
        session.resume()?;
        control_latencies
            .entry("resume")
            .or_default()
            .push(at.elapsed().as_secs_f64() * 1000.0);
    }
    let startup_seconds = start.elapsed().as_secs_f64();
    let active = remedia_resources::collect();
    #[cfg(feature = "experimental-scheduler")]
    let active_scheduled = pool.as_ref().map(|p| p.diagnostics().scheduled_sessions);
    #[cfg(not(feature = "experimental-scheduler"))]
    let active_scheduled: Option<usize> = None;
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    gate.release();
    let processing_start = Instant::now();
    let start_sample = if control_rounds == 0 {
        0
    } else {
        (100 + ((control_rounds - 1) % 4) as u64 * 10) * 48
    };
    let mut next_samples = vec![start_sample; count];
    let mut samples = vec![0_u64; count];
    let mut chunks = vec![0_u64; count];
    let mut checksums = vec![0_u64; count];
    let mut first_chunk_latency_ms = vec![None; count];
    let mut origins: Vec<Option<Instant>> = vec![None; count];
    let mut lateness_ms = Vec::new();
    let mut previous: Vec<Option<(Instant, u32)>> = vec![None; count];
    let mut worst_gap_excess_ms = vec![0.0_f64; count];
    let mut worst_lateness_ms = vec![0.0_f64; count];
    let mut last_chunk_ms = vec![None; count];
    while readers.iter().any(Option::is_some) {
        let mut progress = false;
        for (index, reader) in readers.iter_mut().enumerate() {
            if let Some(active) = reader {
                match active.poll_next(&mut cx) {
                    Poll::Ready(Ok(Some(chunk))) => {
                        let now = Instant::now();
                        let origin = *origins[index].get_or_insert(now);
                        if let Some((previous_time, media_samples)) = previous[index] {
                            let excess = now.duration_since(previous_time).as_secs_f64() * 1000.0
                                - f64::from(media_samples) / 48.0;
                            worst_gap_excess_ms[index] = worst_gap_excess_ms[index].max(excess);
                        }
                        previous[index] = Some((now, chunk.media_samples_per_channel));
                        last_chunk_ms[index] =
                            Some(now.duration_since(processing_start).as_secs_f64() * 1000.0);
                        if first_chunk_latency_ms[index].is_none() {
                            first_chunk_latency_ms[index] =
                                Some(now.duration_since(processing_start).as_secs_f64() * 1000.0);
                        }
                        if pacing == StreamPacing::Realtime {
                            let expected = origin
                                + Duration::from_secs_f64(
                                    (chunk.start_sample - start_sample) as f64 / 48000.0,
                                );
                            let late =
                                now.saturating_duration_since(expected).as_secs_f64() * 1000.0;
                            lateness_ms.push(late);
                            worst_lateness_ms[index] = worst_lateness_ms[index].max(late);
                        }
                        assert_eq!(chunk.start_sample, next_samples[index], "packet continuity");
                        next_samples[index] += u64::from(chunk.media_samples_per_channel);
                        samples[index] += u64::from(chunk.media_samples_per_channel);
                        chunks[index] += 1;
                        for byte in chunk.data {
                            checksums[index] = checksums[index]
                                .wrapping_mul(31)
                                .wrapping_add(u64::from(byte));
                        }
                        progress = true;
                    }
                    Poll::Ready(Ok(None)) => {
                        *reader = None;
                        progress = true;
                    }
                    Poll::Ready(Err(error)) => return Err(error.into()),
                    Poll::Pending => {}
                }
            }
        }
        if !progress {
            std::thread::park_timeout(Duration::from_secs(10));
        }
    }
    let drain_seconds = processing_start.elapsed().as_secs_f64();
    for (index, session) in sessions.iter().enumerate() {
        assert_eq!(session.wait(), PlaybackState::Completed);
        session.stop()?;
        if format.is_none() {
            samples[index] = null_stats[index].snapshot().sample_frames_received;
        }
        assert!(samples[index] > 0);
        assert_eq!(samples[index], samples[index % paths.len()]);
        assert_eq!(checksums[index], checksums[index % paths.len()]);
    }
    let after = remedia_resources::collect();
    let cpu = |s: &remedia_resources::ProcessSnapshot| {
        s.cpu.user_time_ms.unwrap_or(0.0) + s.cpu.system_time_ms.unwrap_or(0.0)
    };
    let audio_seconds = samples.iter().sum::<u64>() as f64 / 48000.0;
    let mut backends = std::collections::BTreeMap::<String, usize>::new();
    for _session in &sessions {
        #[cfg(feature = "experimental-scheduler")]
        let name = _session.executor_diagnostic();
        #[cfg(not(feature = "experimental-scheduler"))]
        let name = "dedicated";
        *backends.entry(name.to_owned()).or_default() += 1;
    }
    #[cfg(feature = "experimental-scheduler")]
    let diagnostics = pool.as_ref().map(|p| {
        let d = p.diagnostics();
        serde_json::json!({"admitted":d.admitted,"scheduledLive":d.scheduled_sessions,"controlFallbacks":d.control_fallbacks,"unsupportedFallbacks":d.unsupported_fallbacks,"fallbackReasons":{"input":d.input_fallbacks,"output":d.output_fallbacks,"demuxer":d.demuxer_fallbacks,"filter":d.filter_fallbacks,"crossfade":d.crossfade_fallbacks},"capacityFallbacks":d.capacity_fallbacks,"actions":d.actions,"turns":d.turns,"queuePeak":d.queue_peak,"queueContentions":d.queue_contentions,"queueWaitNs":d.queue_wait_ns,"deadlineSuspensions":d.deadline_suspensions,"deadlineWakes":d.deadline_wakes,"deadlinePeak":d.deadline_peak,"pendingDeadlines":d.pending_deadlines,"workerBusyNs":d.worker_busy_ns,"controlHandoffs":d.control_handoffs,"controlWorkers":d.control_workers,"startupWorkers":d.startup_workers,"startupQueuePeak":d.startup_queue_peak,"startupRejections":d.startup_rejections,"probeRetries":d.probe_retries,"ioWorkers":d.io_workers,"ioBursts":d.io_bursts,"ioBusyNs":d.io_busy_ns,"ioQueuePeak":d.io_queue_peak})
    });
    #[cfg(not(feature = "experimental-scheduler"))]
    let diagnostics: Option<serde_json::Value> = None;
    lateness_ms.sort_by(f64::total_cmp);
    let delivery_p99_ms = lateness_ms
        .get(lateness_ms.len().saturating_sub(1) * 99 / 100)
        .copied();
    println!(
        "{}",
        serde_json::json!({"sessions":count,"workers":workers,"format":args[4],"audioSeconds":audio_seconds,"wallSeconds":start.elapsed().as_secs_f64(),"startupSeconds":startup_seconds,"processingSeconds":processing_start.elapsed().as_secs_f64(),"startGate":true,"cpuMsPerAudioSecond":(cpu(&after)-cpu(&before))/audio_seconds,"samples":samples,"chunks":chunks,"checksums":checksums,"backends":backends,"diagnostics":diagnostics,"budget":budget,"pacing":format!("{pacing:?}"),"firstChunkLatencyMs":first_chunk_latency_ms,"deliveryP99MsFromFirstChunk":delivery_p99_ms,"deliveryMaxMsFromFirstChunk":lateness_ms.last().copied(),"worstLatenessPerSessionMs":worst_lateness_ms,"worstGapExcessPerSessionMs":worst_gap_excess_ms,"lastChunkMs":last_chunk_ms,"drainSeconds":drain_seconds,"cleanupAfterEofSeconds":processing_start.elapsed().as_secs_f64()-drain_seconds,"controlRounds":control_rounds,"controlLatenciesMs":control_latencies,"controlsSucceeded":true,"inputMode":if file_input {"file"} else {"bytes"},"startupStaggerMs":startup_stagger_ms,"startupLatenciesMs":startup_latencies_ms,"samplingEpochUnixMs":sampling_epoch_unix_ms,"before":before,"active":active,"activeScheduled":active_scheduled,"after":after})
    );
    Ok(())
}
