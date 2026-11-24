//! Rust allocation comparison for standalone streaming versus NullOutput.
//! Usage: stream_output_benchmark FILE null|s16le|f32le|opus [REPETITIONS]
use remedia_core::{
    InputSource, NullOutput, OpusEncoderConfig, PlaybackSession, StreamFormat, StreamOutput,
    StreamPacing,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    time::Instant,
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

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err(
            "usage: stream_output_benchmark FILE null|s16le|f32le|opus [REPETITIONS]".into(),
        );
    }
    let repeats: usize = args
        .get(3)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(3);
    let format = match args[2].as_str() {
        "null" => None,
        "s16le" => Some(StreamFormat::S16le),
        "f32le" => Some(StreamFormat::F32le),
        "opus" => Some(StreamFormat::Opus),
        _ => return Err("unsupported output".into()),
    };
    for repetition in 0..repeats {
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ALLOCATED_BYTES.store(0, Ordering::Relaxed);
        LIVE_BYTES.store(0, Ordering::Relaxed);
        PEAK_LIVE_BYTES.store(0, Ordering::Relaxed);
        let before = remedia_resources::collect();
        let start = Instant::now();
        COUNTING.store(true, Ordering::SeqCst);
        let session = PlaybackSession::new();
        let mut samples = 0_u64;
        let mut chunks = 0_u64;
        let mut payload_bytes = 0_u64;
        if let Some(format) = format {
            let (output, mut reader) =
                StreamOutput::new(format, StreamPacing::Unpaced, OpusEncoderConfig::default())?;
            session.play(InputSource::file(&args[1]), output)?;
            while let Some(chunk) = block_on(reader.next())? {
                samples += u64::from(chunk.media_samples_per_channel);
                chunks += 1;
                payload_bytes += chunk.data.len() as u64;
            }
        } else {
            let output = NullOutput::new();
            let stats = output.stats_handle();
            session.play(InputSource::file(&args[1]), output)?;
            if session.wait() != remedia_core::PlaybackState::Completed {
                return Err(format!("playback failed: {:?}", session.last_error()).into());
            }
            samples = stats.snapshot().sample_frames_received;
        }
        if session.wait() != remedia_core::PlaybackState::Completed {
            return Err(format!("playback failed: {:?}", session.last_error()).into());
        }
        session.stop()?;
        drop(session);
        COUNTING.store(false, Ordering::SeqCst);
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let after = remedia_resources::collect();
        let cpu = |snapshot: &remedia_resources::ProcessSnapshot| {
            snapshot.cpu.user_time_ms.unwrap_or(0.0) + snapshot.cpu.system_time_ms.unwrap_or(0.0)
        };
        let audio_seconds = samples as f64 / 48000.0;
        println!(
            "{}",
            serde_json::json!({
                "repetition": repetition, "format": args[2], "audioSeconds": audio_seconds,
                "wallSeconds": elapsed, "cpuMsPerAudioSecond": (cpu(&after) - cpu(&before)) / audio_seconds,
                "allocations": allocations, "allocatedBytes": allocated_bytes,
                "chunks": chunks, "payloadBytes": payload_bytes,
                "pcmSerializationBytes": if matches!(format, Some(StreamFormat::S16le | StreamFormat::F32le)) { payload_bytes } else { 0 },
                "before": before, "after": after,
            })
        );
    }
    Ok(())
}
