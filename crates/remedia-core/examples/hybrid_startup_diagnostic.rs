//! Diagnostic only: FILE COUNT WORKERS. Stalls probing until a newline arrives on stdin.
//! Byte-stream sources deliberately remain dedicated; these rows are not scheduler performance.
use remedia_core::*;
use std::io::Write;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let bytes = Arc::new(std::fs::read(args.get(1).ok_or("missing fixture")?)?);
    if bytes.len() > 64 * 1024 {
        return Err("fixture must fit the bounded 64 KiB producer queue".into());
    }
    let count: usize = args.get(2).ok_or("missing count")?.parse()?;
    if count == 0 {
        return Err("count must be positive".into());
    }
    let workers: usize = args.get(3).ok_or("missing workers")?.parse()?;
    #[cfg(feature = "experimental-scheduler")]
    let pool = if workers == 0 {
        None
    } else {
        Some(PlaybackScheduler::new(workers, count.max(1))?)
    };
    #[cfg(not(feature = "experimental-scheduler"))]
    if workers != 0 {
        return Err("dedicated-only binary".into());
    }

    let mut sessions = Vec::new();
    let mut tokens = Vec::new();
    let mut writers = Vec::new();
    let mut drivers = Vec::new();
    for index in 0..count {
        #[cfg(feature = "experimental-scheduler")]
        let session = Arc::new(pool.as_ref().map_or_else(PlaybackSession::new, |p| {
            PlaybackSession::with_scheduler(p.clone())
        }));
        #[cfg(not(feature = "experimental-scheduler"))]
        let session = Arc::new(PlaybackSession::new());
        let token = CancellationToken::new();
        let (source, writer) = InputSource::byte_stream(64 * 1024)?;
        sessions.push(session.clone());
        tokens.push(token.clone());
        writers.push(writer);
        let driver = thread::Builder::new()
            .name(format!("hybrid-start-driver-{index}"))
            .stack_size(512 * 1024)
            .spawn(move || -> Result<u64, String> {
                let output = NullOutput::new();
                let stats = output.stats_handle();
                session
                    .play_with_cancellation(source, output, token)
                    .map_err(|e| e.to_string())?;
                let state = session.wait();
                session.stop().map_err(|e| e.to_string())?;
                if state != PlaybackState::Completed {
                    return Err(format!("unexpected state: {state:?}"));
                }
                Ok(stats.snapshot().sample_frames_received)
            });
        match driver {
            Ok(driver) => drivers.push(driver),
            Err(error) => {
                for token in &tokens {
                    token.cancel();
                }
                for session in &sessions {
                    let _ = session.stop();
                }
                for driver in drivers {
                    let _ = driver.join();
                }
                return Err(error.into());
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !sessions
        .iter()
        .all(|s| s.state() == PlaybackState::Starting)
    {
        if Instant::now() >= deadline {
            for token in &tokens {
                token.cancel();
            }
            for session in &sessions {
                let _ = session.stop();
            }
            for driver in drivers {
                let _ = driver.join();
            }
            return Err("startup did not reach the probe barrier".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
    println!("STARTUP_PROBES_WAITING");
    std::io::stdout().flush()?;
    let mut release = String::new();
    std::io::stdin().read_line(&mut release)?;
    let stalled = remedia_resources::collect();
    for writer in writers {
        writer.write(bytes.as_slice())?;
        writer.finish()?;
    }
    let mut samples = Vec::new();
    for driver in drivers {
        samples.push(driver.join().map_err(|_| "driver panicked")??);
    }
    assert!(samples.iter().all(|n| *n > 0 && *n == samples[0]));
    #[cfg(feature = "experimental-scheduler")]
    let diagnostics = pool.as_ref().map(|p| { let d = p.diagnostics(); serde_json::json!({"admitted":d.admitted,"unsupported":d.unsupported_fallbacks,"inputFallbacks":d.input_fallbacks}) });
    #[cfg(not(feature = "experimental-scheduler"))]
    let diagnostics: Option<serde_json::Value> = None;
    println!(
        "{}",
        serde_json::json!({"profilingOnly":true,"scheduledPerformanceEvidence":false,"sessions":count,"workers":workers,"source":"stalled bounded byte stream","callerDriverThreads":count,"stalled":stalled,"after":remedia_resources::collect(),"diagnostics":diagnostics,"samples":samples})
    );
    Ok(())
}
