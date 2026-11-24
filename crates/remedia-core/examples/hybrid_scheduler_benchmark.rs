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
    output: O,
}

impl<O: MediaOutput> MediaOutput for GatedOutput<O> {
    fn wait_ready(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        let mut state = self.gate.state.lock().unwrap();
        while !state.0 && !cancellation.is_cancelled() {
            state = self.gate.changed.wait(state).unwrap();
        }
        drop(state);
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
    fn scheduler_poll_ready(
        &self,
        cx: &mut Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        if cancellation.is_cancelled() {
            return Poll::Ready(Err(OutputError::Cancelled));
        }
        let mut state = self.gate.state.lock().unwrap();
        if !state.0 {
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
        if !state.0 {
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
    let encoded = std::fs::read(args.get(1).ok_or("missing file")?)?;
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
    for _ in 0..count {
        #[cfg(not(feature = "experimental-scheduler"))]
        let session = PlaybackSession::new();
        #[cfg(feature = "experimental-scheduler")]
        let session = pool.as_ref().map_or_else(PlaybackSession::new, |pool| {
            PlaybackSession::with_scheduler(pool.clone())
        });
        // Independent input storage per session; all modes pay the same input ownership cost.
        let source = if args.get(6).map(String::as_str) == Some("file") {
            InputSource::file(&args[1])
        } else {
            InputSource::bytes(encoded.clone())
        };
        if let Some(format) = format {
            let (output, reader) = StreamOutput::new(format, pacing, OpusEncoderConfig::default())?;
            session.play(
                source,
                GatedOutput {
                    gate: gate.clone(),
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
                    output,
                },
            )?;
        }
        sessions.push(session);
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
    let mut samples = vec![0_u64; count];
    let mut chunks = vec![0_u64; count];
    let mut checksums = vec![0_u64; count];
    let mut first_chunk_latency_ms = vec![None; count];
    let mut origins: Vec<Option<Instant>> = vec![None; count];
    let mut lateness_ms = Vec::new();
    while readers.iter().any(Option::is_some) {
        let mut progress = false;
        for (index, reader) in readers.iter_mut().enumerate() {
            if let Some(active) = reader {
                match active.poll_next(&mut cx) {
                    Poll::Ready(Ok(Some(chunk))) => {
                        let now = Instant::now();
                        let origin = *origins[index].get_or_insert(now);
                        if first_chunk_latency_ms[index].is_none() {
                            first_chunk_latency_ms[index] =
                                Some(now.duration_since(processing_start).as_secs_f64() * 1000.0);
                        }
                        if pacing == StreamPacing::Realtime {
                            let expected = origin
                                + Duration::from_secs_f64(chunk.start_sample as f64 / 48000.0);
                            lateness_ms.push(
                                now.saturating_duration_since(expected).as_secs_f64() * 1000.0,
                            );
                        }
                        assert_eq!(chunk.start_sample, samples[index], "packet continuity");
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
    for (index, session) in sessions.iter().enumerate() {
        assert_eq!(session.wait(), PlaybackState::Completed);
        session.stop()?;
        if format.is_none() {
            samples[index] = null_stats[index].snapshot().sample_frames_received;
        }
        assert!(samples[index] > 0);
        assert_eq!(samples[index], samples[0]);
        assert_eq!(checksums[index], checksums[0]);
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
        serde_json::json!({"admitted":d.admitted,"scheduledLive":d.scheduled_sessions,"controlFallbacks":d.control_fallbacks,"unsupportedFallbacks":d.unsupported_fallbacks,"fallbackReasons":{"input":d.input_fallbacks,"output":d.output_fallbacks,"demuxer":d.demuxer_fallbacks,"filter":d.filter_fallbacks,"crossfade":d.crossfade_fallbacks},"capacityFallbacks":d.capacity_fallbacks,"actions":d.actions,"turns":d.turns,"queuePeak":d.queue_peak,"queueContentions":d.queue_contentions,"queueWaitNs":d.queue_wait_ns,"deadlineSuspensions":d.deadline_suspensions,"deadlineWakes":d.deadline_wakes,"deadlinePeak":d.deadline_peak,"pendingDeadlines":d.pending_deadlines})
    });
    #[cfg(not(feature = "experimental-scheduler"))]
    let diagnostics: Option<serde_json::Value> = None;
    lateness_ms.sort_by(f64::total_cmp);
    let delivery_p99_ms = lateness_ms
        .get(lateness_ms.len().saturating_sub(1) * 99 / 100)
        .copied();
    println!(
        "{}",
        serde_json::json!({"sessions":count,"workers":workers,"format":args[4],"audioSeconds":audio_seconds,"wallSeconds":start.elapsed().as_secs_f64(),"startupSeconds":startup_seconds,"processingSeconds":processing_start.elapsed().as_secs_f64(),"startGate":true,"cpuMsPerAudioSecond":(cpu(&after)-cpu(&before))/audio_seconds,"samples":samples,"chunks":chunks,"checksums":checksums,"backends":backends,"diagnostics":diagnostics,"budget":budget,"pacing":format!("{pacing:?}"),"firstChunkLatencyMs":first_chunk_latency_ms,"deliveryP99MsFromFirstChunk":delivery_p99_ms,"samplingEpochUnixMs":sampling_epoch_unix_ms,"before":before,"active":active,"activeScheduled":active_scheduled,"after":after})
    );
    Ok(())
}
