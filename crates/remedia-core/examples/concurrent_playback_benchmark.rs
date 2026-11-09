//! Controlled synchronous sink benchmark; no Discord or external services.
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use remedia_core::{
    CancellationToken, EncodedOpusPacket, InputSource, OpusEncoderConfig, OpusEncoderOutput,
    OpusPacketSink, OutputError, PlaybackSession, PlaybackState,
};

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
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
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

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 5 {
        return Err("usage: concurrent_playback_benchmark FILE SESSIONS passthrough|volume|filter|crossfade paced|unpaced [COMPLEXITY] [controls]".into());
    }
    let count: usize = args[2].parse()?;
    if count == 0 {
        return Err("sessions must be positive".into());
    }
    if !matches!(args[4].as_str(), "paced" | "unpaced") {
        return Err("pacing must be paced or unpaced".into());
    }
    let config = OpusEncoderConfig {
        complexity: args.get(5).map(|v| v.parse()).transpose()?.unwrap_or(9),
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
    let controls = args.get(6).is_some_and(|value| value == "controls");
    let mut commands = Vec::new();
    let started = Instant::now();
    let mut sessions = Vec::new();
    let mut counters = Vec::new();
    for _ in 0..count {
        let session = PlaybackSession::new();
        match args[3].as_str() {
            "passthrough" => {}
            "crossfade" => session.configure_crossfade(Some(
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
        session.play(
            InputSource::file(&args[1]),
            OpusEncoderOutput::new_with_config(sink, config)?,
        )?;
        if controls {
            let command = Instant::now();
            session.pause()?;
            commands.push(command.elapsed().as_micros());
        }
        sessions.push(session);
        counters.push(counts);
    }
    if controls {
        for session in &sessions {
            let command = Instant::now();
            session.seek(Duration::from_millis(100))?;
            commands.push(command.elapsed().as_micros());
            let command = Instant::now();
            session.resume()?;
            commands.push(command.elapsed().as_micros());
        }
    }
    for session in &sessions {
        if session.wait() != PlaybackState::Completed {
            return Err(format!("playback failed: {:?}", session.last_error()).into());
        }
        session.stop()?;
    }
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
            "resource_samples": resources,
        })
    );
    Ok(())
}
