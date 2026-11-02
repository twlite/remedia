use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use remedia_core::{
    CancellationToken, EncodedOpusPacket, InputSource, OpusEncoderOutput, OpusPacketSink,
    OutputError, PlaybackProcessingMode, PlaybackSession, PlaybackState,
};

#[derive(Clone, Default)]
struct PacketCounter(Arc<Mutex<PacketCounts>>);

#[derive(Default)]
struct PacketCounts {
    packets: u64,
    payload_bytes: u64,
    samples_per_channel: u64,
    finished: bool,
}

impl OpusPacketSink for PacketCounter {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        let mut counts = lock(&self.0);
        counts.packets += 1;
        counts.payload_bytes += packet.bytes.len() as u64;
        counts.samples_per_channel += u64::from(packet.media_samples_per_channel);
        Ok(())
    }

    fn flush(
        &self,
        _position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            Err(OutputError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        lock(&self.0).finished = true;
        Ok(())
    }

    fn cancel(&self) {}
}

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os().nth(1).map(PathBuf::from).ok_or(
        "Usage: cargo run -p remedia-core --example opus_processing_benchmark -- <opus-file>",
    )?;

    let cases = [
        ("auto passthrough", None, None),
        ("PCM re-encode", None, Some(0.999)),
        ("light filter", Some("tremolo=f=5:d=0.2"), None),
        ("tempo filter", Some("atempo=1.05"), None),
    ];

    println!("Input: {}", path.display());
    println!(
        "The sink is intentionally unpaced; this measures processing throughput, not live latency."
    );
    for (label, graph, gain) in cases {
        run_case(&path, label, graph, gain)?;
    }
    Ok(())
}

fn run_case(
    path: &Path,
    label: &str,
    graph: Option<&str>,
    gain: Option<f32>,
) -> Result<(), Box<dyn Error>> {
    let session = PlaybackSession::new();
    if let Some(graph) = graph {
        session.set_filter_graph(Some(graph))?;
    }
    if let Some(gain) = gain {
        session.set_volume(gain)?;
    }

    let sink = PacketCounter::default();
    let counts = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new(sink)?;
    let started = Instant::now();
    let info = session.play(InputSource::file(path.to_path_buf()), output)?;
    let state = session.wait();
    let elapsed = started.elapsed();
    if state != PlaybackState::Completed {
        return Err(session
            .last_error()
            .map(|error| Box::new(error) as Box<dyn Error>)
            .unwrap_or_else(|| format!("benchmark case ended in {state:?}").into()));
    }

    let counts = lock(&counts);
    let mode = session.processing_mode();
    let source_seconds = info.duration.map(|duration| duration.as_secs_f64());
    let real_time_factor = source_seconds.map(|seconds| seconds / elapsed.as_secs_f64());
    let sample_seconds = counts.samples_per_channel as f64 / 48_000.0;
    let packet_rate = counts.packets as f64 / elapsed.as_secs_f64();
    let stages = match mode {
        Some(PlaybackProcessingMode::Passthrough) => "decoder=off encoder=off filters=off",
        Some(PlaybackProcessingMode::Transcoding) => "decoder=on encoder=on",
        None => "mode=unknown",
    };

    println!(
        "{label}: mode={mode:?}; {stages}; wall={:.3}s; source={}; delivered={sample_seconds:.3}s; packets={}; payload={}B; packet_rate={packet_rate:.1}/s; realtime_factor={}",
        elapsed.as_secs_f64(),
        source_seconds
            .map(|seconds| format!("{seconds:.3}s"))
            .unwrap_or_else(|| "unknown".to_owned()),
        counts.packets,
        counts.payload_bytes,
        real_time_factor
            .map(|factor| format!("{factor:.2}x"))
            .unwrap_or_else(|| "unknown".to_owned()),
    );
    if !counts.finished {
        return Err("packet sink did not finish cleanly".into());
    }
    drop(counts);
    session.stop()?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
