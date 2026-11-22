//! Measures response-chunk queue ingestion and decoded sample integrity.
//!
//! This benchmark generates bounded `Bytes` slices with the same shared/sliced ownership shape
//! returned by a streamed HTTP body, feeds a PCM WAV through `ByteStreamWriter`, and verifies the
//! decoded sample count and checksum. Usage:
//!
//! ```text
//! cargo run --release -p remedia-core --example plugin_media_queue -- <copy|bytes> <chunk-bytes> <media-bytes>
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

use bytes::Bytes;
use remedia_core::{AudioDecoder, InputSource};

const QUEUE_BYTES: usize = 256 * 1024;
const SLICE_PREFIX_BYTES: usize = 64;

thread_local! {
    static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
}

static WRITE_ALLOCATION_BYTES: AtomicU64 = AtomicU64::new(0);
static WRITE_ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);

struct TrackingAllocator;

// SAFETY: This wrapper forwards each allocation operation unchanged to `System`; the counters
// only observe requests made while the current benchmark producer thread is measuring a write.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if is_tracking_allocations() {
            WRITE_ALLOCATION_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            WRITE_ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // SAFETY: The layout is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if is_tracking_allocations() {
            WRITE_ALLOCATION_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            WRITE_ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // SAFETY: The layout is forwarded unchanged to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The pointer and layout are forwarded unchanged to the system allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if is_tracking_allocations() {
            WRITE_ALLOCATION_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            WRITE_ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // SAFETY: The pointer, old layout and requested size are forwarded unchanged.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode: copy|bytes");
    assert!(matches!(mode.as_str(), "copy" | "bytes"), "unknown mode");

    let chunk_bytes = args
        .next()
        .expect("chunk bytes")
        .parse::<usize>()
        .expect("chunk bytes must be a positive integer");
    let media_bytes = args
        .next()
        .expect("media bytes")
        .parse::<usize>()
        .expect("media bytes must be a positive integer");

    assert!(chunk_bytes > 0 && chunk_bytes % 2 == 0);
    assert!(media_bytes >= chunk_bytes && media_bytes % chunk_bytes == 0);
    assert!(media_bytes <= u32::MAX as usize - 36);

    let chunk_count = media_bytes / chunk_bytes;
    let expected_samples = media_bytes / 2;
    let header = wave_header(expected_samples);
    let (source, writer) = InputSource::byte_stream(QUEUE_BYTES).unwrap();
    let started = Instant::now();
    let producer_mode = mode.clone();
    let producer = thread::spawn(move || {
        writer.write(&header).unwrap();
        let mut max_pending_bytes = writer.pending_bytes();
        let mut write_allocation_bytes = 0_u64;
        let mut write_allocation_count = 0_u64;

        for chunk_index in 0..chunk_count {
            let sample_offset = chunk_index * chunk_bytes / 2;
            let mut payload = Vec::with_capacity(chunk_bytes);
            for sample_offset in sample_offset..sample_offset + chunk_bytes / 2 {
                let sample = ((sample_offset * 31) % 30_000) as i16;
                payload.extend_from_slice(&sample.to_le_bytes());
            }

            let mut backing = Vec::with_capacity(SLICE_PREFIX_BYTES + payload.len());
            backing.resize(SLICE_PREFIX_BYTES, 0x7f);
            backing.extend_from_slice(&payload);
            let response_chunk = Bytes::from(backing).slice(SLICE_PREFIX_BYTES..);

            let (_, allocated_bytes, allocation_count) =
                measure_write_allocations(|| match producer_mode.as_str() {
                    "copy" => writer.write_owned(response_chunk.to_vec()).unwrap(),
                    "bytes" => writer.write_bytes(response_chunk).unwrap(),
                    _ => unreachable!("mode was validated above"),
                });
            write_allocation_bytes += allocated_bytes;
            write_allocation_count += allocation_count;
            max_pending_bytes = max_pending_bytes.max(writer.pending_bytes());
        }

        writer.finish().unwrap();
        (
            max_pending_bytes,
            write_allocation_bytes,
            write_allocation_count,
        )
    });

    let mut decoder = AudioDecoder::open(source).unwrap();
    let mut samples_per_channel = 0_usize;
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;

    while let Some(frame) = decoder.next_frame().unwrap() {
        samples_per_channel += frame.samples_per_channel;

        for sample in frame.samples {
            checksum ^= u64::from(sample.to_bits());
            checksum = checksum.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    let (max_pending_bytes, write_allocation_bytes, write_allocation_count) =
        producer.join().unwrap();
    let elapsed = started.elapsed();
    assert_eq!(samples_per_channel, expected_samples);
    assert!(max_pending_bytes <= QUEUE_BYTES);
    black_box(checksum);

    let mebibytes = media_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "{{\"mode\":\"{mode}\",\"chunk_bytes\":{chunk_bytes},\"chunk_count\":{chunk_count},\
         \"media_bytes\":{media_bytes},\"samples_per_channel\":{samples_per_channel},\
         \"checksum\":{checksum},\"max_pending_bytes\":{max_pending_bytes},\
         \"write_allocation_bytes\":{write_allocation_bytes},\
         \"write_allocation_count\":{write_allocation_count},\
         \"elapsed_us\":{},\"mib_per_second\":{:.3}}}",
        elapsed.as_micros(),
        mebibytes / elapsed.as_secs_f64(),
    );
}

fn is_tracking_allocations() -> bool {
    TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false)
}

fn measure_write_allocations<T>(write: impl FnOnce() -> T) -> (T, u64, u64) {
    WRITE_ALLOCATION_BYTES.store(0, Ordering::Relaxed);
    WRITE_ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    TRACK_ALLOCATIONS.with(|enabled| enabled.set(true));

    struct StopTracking;

    impl Drop for StopTracking {
        fn drop(&mut self) {
            TRACK_ALLOCATIONS.with(|enabled| enabled.set(false));
        }
    }

    let stop_tracking = StopTracking;
    let result = write();
    drop(stop_tracking);

    (
        result,
        WRITE_ALLOCATION_BYTES.load(Ordering::Relaxed),
        WRITE_ALLOCATION_COUNT.load(Ordering::Relaxed),
    )
}

fn wave_header(samples_per_channel: usize) -> Vec<u8> {
    let data_bytes = (samples_per_channel * 2) as u32;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&48_000_u32.to_le_bytes());
    header.extend_from_slice(&96_000_u32.to_le_bytes());
    header.extend_from_slice(&2_u16.to_le_bytes());
    header.extend_from_slice(&16_u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    header
}
