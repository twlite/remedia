//! Release-only controlled comparison: FILE SESSIONS WORKERS FORMAT.
//! WORKERS=0 selects dedicated execution; FORMAT is null, s16le, f32le or opus.
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
            let (output, reader) =
                StreamOutput::new(format, StreamPacing::Unpaced, OpusEncoderConfig::default())?;
            session.play(source, output)?;
            readers.push(Some(reader));
        } else {
            let output = NullOutput::new();
            null_stats.push(output.stats_handle());
            session.play(source, output)?;
        }
        sessions.push(session);
    }
    let startup_seconds = start.elapsed().as_secs_f64();
    let active = remedia_resources::collect();
    #[cfg(feature = "experimental-scheduler")]
    let active_scheduled = pool.as_ref().map(|p| p.diagnostics().scheduled_sessions);
    #[cfg(not(feature = "experimental-scheduler"))]
    let active_scheduled: Option<usize> = None;
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut samples = vec![0_u64; count];
    let mut chunks = vec![0_u64; count];
    let mut checksums = vec![0_u64; count];
    while readers.iter().any(Option::is_some) {
        let mut progress = false;
        for (index, reader) in readers.iter_mut().enumerate() {
            if let Some(active) = reader {
                match active.poll_next(&mut cx) {
                    Poll::Ready(Ok(Some(chunk))) => {
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
        serde_json::json!({"admitted":d.admitted,"scheduledLive":d.scheduled_sessions,"controlFallbacks":d.control_fallbacks,"unsupportedFallbacks":d.unsupported_fallbacks,"fallbackReasons":{"input":d.input_fallbacks,"output":d.output_fallbacks,"demuxer":d.demuxer_fallbacks,"filter":d.filter_fallbacks,"crossfade":d.crossfade_fallbacks},"capacityFallbacks":d.capacity_fallbacks,"actions":d.actions,"turns":d.turns,"queuePeak":d.queue_peak,"queueContentions":d.queue_contentions,"queueWaitNs":d.queue_wait_ns})
    });
    #[cfg(not(feature = "experimental-scheduler"))]
    let diagnostics: Option<serde_json::Value> = None;
    println!(
        "{}",
        serde_json::json!({"sessions":count,"workers":workers,"format":args[4],"audioSeconds":audio_seconds,"wallSeconds":start.elapsed().as_secs_f64(),"startupSeconds":startup_seconds,"cpuMsPerAudioSecond":(cpu(&after)-cpu(&before))/audio_seconds,"samples":samples,"chunks":chunks,"checksums":checksums,"backends":backends,"diagnostics":diagnostics,"budget":budget,"samplingEpochUnixMs":sampling_epoch_unix_ms,"before":before,"active":active,"activeScheduled":active_scheduled,"after":after})
    );
    Ok(())
}
