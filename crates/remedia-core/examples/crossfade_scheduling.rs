//! Deterministic scheduler cost/allocation benchmark (no media/network IO).
//! Run with `cargo run --release -p remedia-core --example crossfade_scheduling`.
mod analysis {
    pub const HOP_FRAMES: usize = 480;
}
#[path = "../src/crossfade/scheduling.rs"]
mod scheduling;

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct Allocator;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forward the caller's allocator contract unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: this pointer/layout was allocated by System above.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn cpu_ms() -> f64 {
    let cpu = remedia_resources::collect().cpu;
    cpu.user_time_ms.unwrap_or(0.0) + cpu.system_time_ms.unwrap_or(0.0)
}

fn main() {
    let mut pcm = vec![0.1; 10 * 48_000 * 2];
    pcm.extend(vec![0.0; 10 * 48_000 * 2]);
    let iterations = 500;
    let cpu_before = cpu_ms();
    let start = Instant::now();
    let mut construction_allocations = 0;
    let mut processing_allocations = 0;
    let mut endpoint = 0;
    for _ in 0..iterations {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let mut analyzer = scheduling::TailAnalysis::new();
        construction_allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for block in pcm.chunks(1920) {
            analyzer.push(black_box(block));
        }
        endpoint = black_box(analyzer.endpoint().unwrap());
        black_box(scheduling::leading_trim(&pcm[..2 * 48_000]));
        processing_allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = cpu_ms() - cpu_before;
    println!(
        "audio_seconds={} wall_ms={:.3} cpu_ms={:.3} ns_per_frame={:.3} construction_allocations_per_probe={} processing_allocations={} endpoint_ms={} retained_after_audible_ms={}",
        iterations * 20,
        wall * 1000.0,
        cpu,
        wall * 1e9 / (iterations as f64 * 20.0 * 48_000.0),
        construction_allocations / iterations,
        processing_allocations,
        endpoint / 48,
        endpoint / 48 - 10_000
    );
    assert_eq!(processing_allocations, 0);
    assert!((10_250..10_800).contains(&(endpoint / 48)));
}
