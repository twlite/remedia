use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use remedia_observability::{Observability, ObservabilityConfig};

use super::{
    MediaPositionAccuracy, PlaybackError, PlaybackProcessingMode, PlaybackSession, PlaybackState,
    SessionData, SessionShared, lock, wait,
};
use crate::decoder::{AudioDecoder, DecoderOptions, PcmFrame};
use crate::opus::{EncodedOpusPacket, OpusEncoderOutput, OpusPacketSink};
use crate::output::{MediaOutput, NullOutput, NullOutputStats, OutputError};
use crate::source::{CancellationToken, InputSource};

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);
// Playback worker tests coordinate several threads and native FFmpeg
// operations. Leave enough room for loaded hosted runners while retaining
// a finite bound so genuine deadlocks still fail deterministically.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

struct TempWave {
    directory: PathBuf,
    path: PathBuf,
}

impl TempWave {
    fn constant(sample_count: usize, sample: i16) -> Self {
        Self::from_samples(vec![sample; sample_count])
    }

    fn from_samples(samples: Vec<i16>) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("remedia-playback-{}-{id}", std::process::id()));
        fs::create_dir_all(&directory).expect("create playback test directory");
        let path = directory.join("source.wav");
        fs::write(&path, pcm_wave(&samples)).expect("write playback fixture");
        Self { directory, path }
    }

    fn source(&self) -> InputSource {
        InputSource::file(&self.path)
    }
}

impl Drop for TempWave {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[derive(Clone)]
struct GateOutput(Arc<GateShared>);

#[derive(Clone)]
struct GateHandle(Arc<GateShared>);

struct GateShared {
    data: Mutex<GateData>,
    changed: Condvar,
}

#[derive(Default)]
struct GateData {
    permits: usize,
    write_calls: usize,
    accepted: Vec<(u64, f32)>,
    flushes: Vec<Duration>,
    position: Duration,
    cancelled: bool,
    finish_blocked: bool,
    finish_started: bool,
    finished: bool,
}

impl GateOutput {
    fn new() -> (Self, GateHandle) {
        let shared = Arc::new(GateShared {
            data: Mutex::new(GateData::default()),
            changed: Condvar::new(),
        });
        (Self(Arc::clone(&shared)), GateHandle(shared))
    }

    fn blocked_finish() -> (Self, GateHandle) {
        let (output, handle) = Self::new();
        lock(&handle.0.data).finish_blocked = true;
        (output, handle)
    }
}

impl GateHandle {
    fn release(&self, count: usize) {
        let mut data = lock(&self.0.data);
        data.permits = data.permits.saturating_add(count);
        self.0.changed.notify_all();
    }

    fn wait_for_calls(&self, count: usize) -> bool {
        let data = lock(&self.0.data);
        let (data, _) = self
            .0
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                data.write_calls < count && !data.cancelled
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.write_calls >= count
    }

    fn wait_for_accepted(&self, count: usize) -> bool {
        let data = lock(&self.0.data);
        let (data, _) = self
            .0
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                data.accepted.len() < count && !data.cancelled
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.accepted.len() >= count
    }

    fn wait_for_finish(&self) -> bool {
        let data = lock(&self.0.data);
        let (data, _) = self
            .0
            .changed
            .wait_timeout_while(data, TEST_TIMEOUT, |data| {
                !data.finish_started && !data.cancelled
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.finish_started
    }

    fn release_finish(&self) {
        let mut data = lock(&self.0.data);
        data.finish_blocked = false;
        self.0.changed.notify_all();
    }

    fn snapshot(&self) -> GateSnapshot {
        let data = lock(&self.0.data);
        GateSnapshot {
            write_calls: data.write_calls,
            accepted: data.accepted.clone(),
            flushes: data.flushes.clone(),
            cancelled: data.cancelled,
            finished: data.finished,
        }
    }
}

#[derive(Debug)]
struct GateSnapshot {
    write_calls: usize,
    accepted: Vec<(u64, f32)>,
    flushes: Vec<Duration>,
    cancelled: bool,
    finished: bool,
}

impl MediaOutput for GateOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut data = lock(&self.0.data);
        data.write_calls += 1;
        self.0.changed.notify_all();
        while data.permits == 0 && !data.cancelled && !cancellation.is_cancelled() {
            data = wait(&self.0.changed, data);
        }
        if data.cancelled || cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        data.permits -= 1;
        let first_sample = frame.samples.first().copied().unwrap_or_default();
        data.position = samples_to_duration(
            frame
                .start_sample
                .saturating_add(frame.samples_per_channel as u64),
        );
        data.accepted.push((frame.start_sample, first_sample));
        self.0.changed.notify_all();
        Ok(())
    }

    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        let mut data = lock(&self.0.data);
        if data.cancelled {
            return Err(OutputError::Cancelled);
        }
        data.position = position;
        data.flushes.push(position);
        self.0.changed.notify_all();
        Ok(())
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        let mut data = lock(&self.0.data);
        data.finish_started = true;
        self.0.changed.notify_all();
        while data.finish_blocked && !data.cancelled && !cancellation.is_cancelled() {
            data = wait(&self.0.changed, data);
        }
        if data.cancelled || cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        data.finished = true;
        self.0.changed.notify_all();
        Ok(())
    }

    fn cancel(&self) {
        let mut data = lock(&self.0.data);
        data.cancelled = true;
        self.0.changed.notify_all();
    }

    fn played_position(&self) -> Option<Duration> {
        Some(lock(&self.0.data).position)
    }
}

struct FailingOutput(Arc<AtomicBool>);

impl MediaOutput for FailingOutput {
    fn write_frame(
        &self,
        _frame: PcmFrame,
        _cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        Err(OutputError::new("controlled output failure"))
    }

    fn flush(
        &self,
        _position: Duration,
        _cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        Ok(())
    }

    fn finish(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

fn pause_at_first_blocked_frame(session: &Arc<PlaybackSession>, gate: &GateHandle) {
    assert!(gate.wait_for_calls(1));
    let pause_session = Arc::clone(session);
    let pause_thread = thread::spawn(move || pause_session.pause());
    assert!(wait_for_state(session, PlaybackState::Pausing));
    gate.release(1);
    assert!(pause_thread.join().unwrap().is_ok());
    assert_eq!(session.state(), PlaybackState::Paused);
}

fn wait_for_state(session: &PlaybackSession, expected: PlaybackState) -> bool {
    wait_for_condition(&session.shared, TEST_TIMEOUT, |data| data.state == expected)
}

fn wait_for_volume(session: &PlaybackSession, expected: f32) -> bool {
    wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
        data.volume == expected
    })
}

fn wait_for_processing_mode(session: &PlaybackSession, expected: PlaybackProcessingMode) -> bool {
    wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
        data.processing_mode == Some(expected)
    })
}

fn wait_for_pending_seek(session: &PlaybackSession) -> bool {
    wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
        data.pending_seek.is_some()
    })
}

fn wait_for_pending_filter_update(session: &PlaybackSession) -> bool {
    let data = lock(&session.shared.data);
    let (data, _) = session
        .shared
        .changed
        .wait_timeout_while(data, TEST_TIMEOUT, |data| {
            data.pending_filter_graph.is_none()
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    data.pending_filter_graph.is_some()
}

fn replace_graph_while_packet_sender_is_blocked(
    session: Arc<PlaybackSession>,
    gate: StepPacketHandle,
    expression: Option<&'static str>,
) {
    let updates_before = gate.filter_update_completions();
    let update_gate = gate.clone();
    let worker_session = Arc::clone(&session);
    let update_worker = thread::spawn(move || {
        let result = worker_session
            .set_filter_graph(expression)
            .map_err(|error| error.to_string());
        update_gate.record_filter_update(result);
    });
    // The worker may consume pending_filter_graph before this thread
    // reacquires the session lock. Observe the durable requested graph,
    // which set_filter_graph writes at the same time it queues the update,
    // instead of requiring a transient pending slot to remain populated.
    if !wait_for_condition(&session.shared, TEST_TIMEOUT, |data| {
        data.filter_graph.as_deref() == expression
    }) {
        let diagnostics = gate.transition_diagnostics(&session, updates_before);
        let _ = session.stop();
        let _ = update_worker.join();
        panic!("filtergraph replacement was never accepted; {diagnostics}");
    }
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut after_calls = 0;
    let mut released_packets = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let progress =
            gate.wait_for_update_or_blocked_packet(after_calls, updates_before, remaining);
        match progress {
            StepSinkProgress::FilterUpdateCompleted(result) => {
                update_worker.join().expect("filter update worker joins");
                assert!(
                    result.is_ok(),
                    "filter replacement failed: {result:?}; {}",
                    gate.transition_diagnostics(&session, updates_before)
                );
                return;
            }
            StepSinkProgress::PacketBlocked(call) => {
                if released_packets == 128 {
                    let diagnostics = gate.transition_diagnostics(&session, updates_before);
                    let _ = session.stop();
                    let _ = update_worker.join();
                    panic!(
                        "filtergraph transition remained pending after 128 blocked packets; {diagnostics}"
                    );
                }
                gate.release(1);
                after_calls = call;
                released_packets += 1;
            }
            StepSinkProgress::Cancelled => {
                let diagnostics = gate.transition_diagnostics(&session, updates_before);
                let _ = session.stop();
                let _ = update_worker.join();
                panic!("sink was cancelled before filtergraph transition completed; {diagnostics}");
            }
            StepSinkProgress::TimedOut => {
                let diagnostics = gate.transition_diagnostics(&session, updates_before);
                let _ = session.stop();
                let _ = update_worker.join();
                panic!("timed out waiting for filtergraph transition; {diagnostics}");
            }
        }
    }
}

fn wait_for_condition(
    shared: &SessionShared,
    timeout: Duration,
    condition: impl Fn(&SessionData) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut data = lock(&shared.data);
    while !condition(&data) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let result = shared.changed.wait_timeout(data, remaining);
        match result {
            Ok((next, wait_result)) => {
                data = next;
                if wait_result.timed_out() && !condition(&data) {
                    return false;
                }
            }
            Err(poisoned) => data = poisoned.into_inner().0,
        }
    }
    true
}

fn pcm_wave(samples: &[i16]) -> Vec<u8> {
    let channels = 1_u16;
    let sample_rate = 48_000_u32;
    let bytes_per_sample = 2_u16;
    let block_align = channels * bytes_per_sample;
    let data_len = samples.len() * usize::from(bytes_per_sample);
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    push_u32(&mut bytes, (36 + data_len) as u32);
    bytes.extend_from_slice(b"WAVEfmt ");
    push_u32(&mut bytes, 16);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, channels);
    push_u32(&mut bytes, sample_rate);
    push_u32(&mut bytes, sample_rate * u32::from(block_align));
    push_u16(&mut bytes, block_align);
    push_u16(&mut bytes, 16);
    bytes.extend_from_slice(b"data");
    push_u32(&mut bytes, data_len as u32);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn samples_to_duration(samples: u64) -> Duration {
    let seconds = samples / 48_000;
    let remainder = samples % 48_000;
    let nanoseconds = (u128::from(remainder) * 1_000_000_000 / 48_000) as u32;
    Duration::new(seconds, nanoseconds)
}

mod lifecycle;

mod seeking;

mod processing;

mod crossfade;

mod packet_sink;
use packet_sink::*;
