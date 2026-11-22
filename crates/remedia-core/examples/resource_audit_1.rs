//! Compare blocking completion waits with runtime-independent async waiters.
//!
//! Usage: `resource_audit_1 MODE WAITERS [alloc]`, where MODE is `blocking` or `async`.
//! The input is a generated 20 ms WAV and the sink gates its first frame so all
//! waiters are pending at the same time. See `benchmarks/resource-audit-1.md`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::VecDeque;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use remedia_core::{
    CancellationToken, InputSource, MediaOutput, OutputError, PcmFrame, PlaybackSession,
    PlaybackState,
};
use remedia_resources::collect;
use serde_json::{Value, json};

const SAMPLE_FRAMES: usize = 960;
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

struct CountingAllocator;

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the layout is forwarded unchanged to the system allocator.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() && COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
        }

        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            subtract_live_bytes(layout.size());
        }

        // SAFETY: pointer and layout match the allocation passed to this deallocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the pointer and old layout are forwarded unchanged to the system allocator.
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() && COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            if new_size >= layout.size() {
                let increase = new_size - layout.size();
                let live = LIVE_BYTES.fetch_add(increase, Ordering::Relaxed) + increase;
                PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
            } else {
                let decrease = layout.size() - new_size;
                subtract_live_bytes(decrease);
            }
        }

        resized
    }
}

fn subtract_live_bytes(amount: usize) {
    let mut current = LIVE_BYTES.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_sub(amount);
        match LIVE_BYTES.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

struct GateState {
    entered: bool,
    released: bool,
    cancelled: bool,
    frames: u64,
    sample_frames: u64,
    finished: bool,
    finish_at: Option<Instant>,
}

struct GateOutput(Arc<(Mutex<GateState>, Condvar)>);

struct OutputGate(Arc<(Mutex<GateState>, Condvar)>);

impl OutputGate {
    fn release(&self) {
        let (state, changed) = &*self.0;
        let mut state = lock(state);
        state.released = true;
        changed.notify_all();
    }

    fn wait_until_entered(&self) -> Result<(), Box<dyn Error>> {
        let (state, changed) = &*self.0;
        let state = lock(state);
        let (state, timeout) = changed
            .wait_timeout_while(state, COMPLETION_TIMEOUT, |state| !state.entered)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if timeout.timed_out() && !state.entered {
            return Err("playback did not reach the gated output frame".into());
        }

        Ok(())
    }

    fn result(&self) -> (u64, u64, bool, Option<Instant>) {
        let (state, _) = &*self.0;
        let state = lock(state);
        (
            state.frames,
            state.sample_frames,
            state.finished,
            state.finish_at,
        )
    }
}

impl GateOutput {
    fn new() -> (Self, OutputGate) {
        let shared = Arc::new((
            Mutex::new(GateState {
                entered: false,
                released: false,
                cancelled: false,
                frames: 0,
                sample_frames: 0,
                finished: false,
                finish_at: None,
            }),
            Condvar::new(),
        ));

        (Self(Arc::clone(&shared)), OutputGate(shared))
    }
}

impl MediaOutput for GateOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let (state, changed) = &*self.0;
        let mut state = lock(state);
        state.entered = true;
        changed.notify_all();

        while !state.released && !state.cancelled && !cancellation.is_cancelled() {
            state = changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }

        if state.cancelled || cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }

        state.frames += 1;
        state.sample_frames += frame.samples_per_channel as u64;
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

        let (state, changed) = &*self.0;
        let mut state = lock(state);
        state.finished = true;
        state.finish_at = Some(Instant::now());
        changed.notify_all();
        Ok(())
    }

    fn cancel(&self) {
        let (state, changed) = &*self.0;
        lock(state).cancelled = true;
        changed.notify_all();
    }
}

struct TaskQueue {
    ready: Mutex<QueueData>,
    changed: Condvar,
}

struct QueueData {
    ready: VecDeque<usize>,
    queued: Vec<bool>,
}

impl TaskQueue {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            ready: Mutex::new(QueueData {
                ready: VecDeque::with_capacity(capacity),
                queued: vec![false; capacity],
            }),
            changed: Condvar::new(),
        }
    }

    fn wake(&self, index: usize) {
        let mut ready = lock(&self.ready);
        if !ready.queued[index] {
            ready.queued[index] = true;
            ready.ready.push_back(index);
        }
        self.changed.notify_one();
    }

    fn take(&self) -> Result<usize, Box<dyn Error>> {
        let ready = lock(&self.ready);
        let (mut ready, timeout) = self
            .changed
            .wait_timeout_while(ready, COMPLETION_TIMEOUT, |queue| queue.ready.is_empty())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if timeout.timed_out() && ready.ready.is_empty() {
            return Err("async completion executor timed out".into());
        }

        let index = ready
            .ready
            .pop_front()
            .ok_or("async queue woke without a task")?;
        ready.queued[index] = false;
        Ok(index)
    }
}

struct TaskWake {
    queue: Arc<TaskQueue>,
    index: usize,
}

impl Wake for TaskWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.queue.wake(self.index);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn resource_values() -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let snapshot = collect();
    (
        snapshot.cpu.thread_count,
        snapshot.memory.resident_bytes,
        snapshot.memory.footprint_bytes,
        snapshot.memory.peak_resident_bytes,
    )
}

fn live_heap_bytes() -> Option<usize> {
    COUNT_ALLOCATIONS
        .load(Ordering::Relaxed)
        .then(|| LIVE_BYTES.load(Ordering::Relaxed))
}

fn run(mode: &str, waiter_count: usize) -> Result<Value, Box<dyn Error>> {
    if waiter_count == 0 {
        return Err("waiter count must be positive".into());
    }

    let source = InputSource::bytes(wave(SAMPLE_FRAMES));
    let session = PlaybackSession::new();
    let (output, gate) = GateOutput::new();
    session.play(source, output)?;
    gate.wait_until_entered()?;

    let waiter = session.completion();
    let (
        thread_count_pending,
        resident_pending,
        footprint_pending,
        rust_heap_pending,
        completion_us,
        finish_to_waiters_us,
        wakers_pending,
        wakers_completed,
    ) = match mode {
        "blocking" => {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (state_tx, state_rx) = mpsc::channel();
            let mut threads = Vec::with_capacity(waiter_count);

            for _ in 0..waiter_count {
                let waiter = Arc::new(session.completion());
                let ready = ready_tx.clone();
                let states = state_tx.clone();
                threads.push(thread::spawn(move || {
                    ready.send(()).unwrap();
                    states.send(waiter.wait()).unwrap();
                }));
            }
            drop(ready_tx);
            drop(state_tx);

            for _ in 0..waiter_count {
                ready_rx.recv_timeout(COMPLETION_TIMEOUT)?;
            }
            thread::sleep(Duration::from_millis(10));
            let (thread_count, resident, footprint, _) = resource_values();
            let rust_heap_pending = live_heap_bytes();

            let started = Instant::now();
            gate.release();
            for _ in 0..waiter_count {
                if state_rx.recv_timeout(COMPLETION_TIMEOUT)? != PlaybackState::Completed {
                    return Err("blocking waiter returned a non-completed state".into());
                }
            }
            for waiter_thread in threads {
                waiter_thread
                    .join()
                    .map_err(|_| "blocking waiter thread panicked")?;
            }

            let completion_us = started.elapsed().as_secs_f64() * 1_000_000.0;
            let finish_at = gate
                .result()
                .3
                .ok_or("output did not reach successful finish")?;
            let finish_to_waiters_us = finish_at.elapsed().as_secs_f64() * 1_000_000.0;
            (
                thread_count,
                resident,
                footprint,
                rust_heap_pending,
                completion_us,
                finish_to_waiters_us,
                Some(0_usize),
                Some(0_usize),
            )
        }
        "async" => {
            let queue = Arc::new(TaskQueue::with_capacity(waiter_count));
            let signals = (0..waiter_count)
                .map(|index| {
                    Arc::new(TaskWake {
                        queue: Arc::clone(&queue),
                        index,
                    })
                })
                .collect::<Vec<_>>();
            let wakers = signals
                .iter()
                .map(|signal| Waker::from(Arc::clone(signal)))
                .collect::<Vec<_>>();
            let mut futures = (0..waiter_count)
                .map(|_| Box::pin(waiter.wait_async()))
                .collect::<Vec<_>>();

            for (future, waker) in futures.iter_mut().zip(&wakers) {
                if poll(future.as_mut(), waker) != Poll::Pending {
                    return Err("async waiter completed before the output was released".into());
                }
            }

            let retained_pending = signals
                .iter()
                .map(|signal| Arc::strong_count(signal).saturating_sub(2))
                .sum::<usize>();
            let (thread_count, resident, footprint, _) = resource_values();
            let rust_heap_pending = live_heap_bytes();

            let started = Instant::now();
            gate.release();
            let mut states = vec![None; waiter_count];
            let mut completed = 0;
            while completed < waiter_count {
                let index = queue.take()?;
                if states[index].is_some() {
                    continue;
                }

                if let Poll::Ready(state) = poll(futures[index].as_mut(), &wakers[index]) {
                    if state != PlaybackState::Completed {
                        return Err("async waiter returned a non-completed state".into());
                    }
                    states[index] = Some(state);
                    completed += 1;
                }
            }

            let completion_us = started.elapsed().as_secs_f64() * 1_000_000.0;
            let finish_at = gate
                .result()
                .3
                .ok_or("output did not reach successful finish")?;
            let finish_to_waiters_us = finish_at.elapsed().as_secs_f64() * 1_000_000.0;
            let retained_completed = signals
                .iter()
                .map(|signal| Arc::strong_count(signal).saturating_sub(2))
                .sum::<usize>();
            if retained_completed != 0 {
                return Err(
                    format!("async completion retained {retained_completed} task wakers").into(),
                );
            }

            // Ready futures no longer own registry entries. Drop the task values
            // before the post-case resource snapshot below.
            drop(futures);
            drop(wakers);
            drop(signals);

            (
                thread_count,
                resident,
                footprint,
                rust_heap_pending,
                completion_us,
                finish_to_waiters_us,
                Some(retained_pending),
                Some(retained_completed),
            )
        }
        _ => return Err("mode must be `blocking` or `async`".into()),
    };

    let (frames, sample_frames, output_finished, _) = gate.result();
    if !output_finished || sample_frames != SAMPLE_FRAMES as u64 {
        return Err(format!(
            "sample/output check failed: finished={output_finished}, frames={sample_frames}, expected={SAMPLE_FRAMES}"
        )
        .into());
    }

    if session.state() != PlaybackState::Completed {
        return Err("session did not publish Completed".into());
    }
    session.stop()?;

    drop(waiter);
    drop(gate);
    drop(session);

    let (thread_count_after_drop, resident_after_drop, footprint_after_drop, peak_resident) =
        resource_values();
    let rust_heap_after_drop = live_heap_bytes();
    let cpu_snapshot = collect().cpu;
    let cpu_ms = cpu_snapshot
        .user_time_ms
        .zip(cpu_snapshot.system_time_ms)
        .map(|(user_ms, system_ms)| user_ms + system_ms);

    Ok(json!({
        "mode": mode,
        "waiters": waiter_count,
        "threadCountPending": thread_count_pending,
        "threadCountAfterDrop": thread_count_after_drop,
        "residentBytesPending": resident_pending,
        "residentBytesAfterDrop": resident_after_drop,
        "footprintBytesPending": footprint_pending,
        "footprintBytesAfterDrop": footprint_after_drop,
        "peakResidentBytes": peak_resident,
        "rustHeapLivePendingBytes": rust_heap_pending,
        "rustHeapLiveAfterDropBytes": rust_heap_after_drop,
        "rustHeapPeakLiveBytes": COUNT_ALLOCATIONS.load(Ordering::Relaxed).then(|| PEAK_LIVE_BYTES.load(Ordering::Relaxed)),
        "completionUs": completion_us,
        "outputFinishToAllWaitersUs": finish_to_waiters_us,
        "releaseToAllWaitersUs": completion_us,
        "futureWakerRegistrationsPending": if mode == "async" { Some(waiter_count) } else { Some(0) },
        "futureWakerRegistrationsAfterCompletion": 0,
        "retainedTaskWakersPending": wakers_pending,
        "retainedTaskWakersAfterCompletion": wakers_completed,
        "completedWaiters": waiter_count,
        "outputFrames": frames,
        "sampleFramesPerChannel": sample_frames,
        "outputFinished": output_finished,
        "processCpuMs": cpu_ms,
        "nativeHeapBytes": Value::Null,
    }))
}

fn poll<F: Future<Output = PlaybackState>>(
    future: Pin<&mut F>,
    waker: &Waker,
) -> Poll<PlaybackState> {
    future.poll(&mut Context::from_waker(waker))
}

fn wave(sample_frames: usize) -> Vec<u8> {
    let data_bytes = sample_frames * 2;
    let mut bytes = Vec::with_capacity(44 + data_bytes);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36_u32 + data_bytes as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for sample in 0..sample_frames {
        let value = if sample % 2 == 0 { 4096_i16 } else { -4096_i16 };
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    bytes
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().collect::<Vec<_>>();
    let mode = arguments.get(1).map(String::as_str).unwrap_or("async");
    let waiter_count = arguments
        .get(2)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1);
    COUNT_ALLOCATIONS.store(
        arguments.get(3).is_some_and(|value| value == "alloc"),
        Ordering::Relaxed,
    );

    let result = run(mode, waiter_count)?;
    println!("{result}");
    Ok(())
}
