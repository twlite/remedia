//! Bounded staged PCM buffering for the incoming source of a transition.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::decoder::OUTPUT_SAMPLE_RATE_HZ;

/// A bounded, cancellation-aware PCM staging buffer.
///
/// A source decoder can fill this buffer independently while the current track
/// continues to output audio. Staging never sends packets to the voice sink.
/// When full, producers block until the mixer consumes audio or the stage is
/// invalidated. A consumer never blocks the real-time output thread.
pub struct StagedPcmBuffer {
    capacity_scalars: usize,
    growth_chunk_scalars: usize,
    state: Mutex<StagedPcmState>,
    writable: Condvar,
    readable: Condvar,
}

#[derive(Default)]
struct StagedPcmState {
    pcm: VecDeque<f32>,
    finished: bool,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageBufferError {
    InvalidFrame,
    Cancelled,
    Closed,
}

impl StagedPcmBuffer {
    /// Construct a bounded buffer of normalized 48 kHz stereo PCM.
    /// The recommended capacity is 2 seconds, approximately 768 KiB.
    pub fn with_capacity_ms(capacity_ms: u32) -> Option<Self> {
        if !(100..=10_000).contains(&capacity_ms) {
            return None;
        }
        let capacity_scalars =
            usize::try_from(u64::from(capacity_ms) * u64::from(OUTPUT_SAMPLE_RATE_HZ) * 2 / 1_000)
                .ok()?;
        let growth_chunk_scalars =
            usize::try_from(u64::from(OUTPUT_SAMPLE_RATE_HZ) * 2 * 500 / 1_000)
                .ok()?
                .min(capacity_scalars);

        Some(Self {
            capacity_scalars,
            growth_chunk_scalars,
            // Allocate staging PCM only as decoding fills the bounded buffer.
            state: Mutex::new(StagedPcmState::default()),
            writable: Condvar::new(),
            readable: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, StagedPcmState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reserve_for(&self, state: &mut StagedPcmState, required_scalars: usize) {
        if state.pcm.capacity() >= required_scalars {
            return;
        }

        let target_capacity = required_scalars
            .div_ceil(self.growth_chunk_scalars)
            .saturating_mul(self.growth_chunk_scalars)
            .min(self.capacity_scalars);
        state
            .pcm
            .reserve_exact(target_capacity.saturating_sub(state.pcm.len()));
    }

    /// Submit interleaved stereo frames, blocking only the source decoder
    /// worker on backpressure. Calling cancel releases blocked producers.
    pub fn push(&self, frame: &[f32]) -> Result<(), StageBufferError> {
        if !frame.len().is_multiple_of(2) {
            return Err(StageBufferError::InvalidFrame);
        }
        let mut remaining = frame;
        while !remaining.is_empty() {
            let mut state = self.lock();
            while state.pcm.len() == self.capacity_scalars && !state.cancelled && !state.finished {
                state = self
                    .writable
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.cancelled {
                return Err(StageBufferError::Cancelled);
            }
            if state.finished {
                return Err(StageBufferError::Closed);
            }
            let space = self.capacity_scalars - state.pcm.len();
            // The capacity is even, and consumers only read stereo pairs.
            let count = remaining.len().min(space);
            let required_scalars = state.pcm.len() + count;
            self.reserve_for(&mut state, required_scalars);
            state.pcm.extend(remaining[..count].iter().copied());
            remaining = &remaining[count..];
            self.readable.notify_all();
        }
        Ok(())
    }

    /// Pop exactly `samples_per_channel` frames into `destination`, replacing its contents and
    /// reusing its allocation. Returns false without changing `destination` when not enough
    /// audio is ready, or when the request exceeds the buffer's capacity.
    pub fn try_pop_exact_into(
        &self,
        samples_per_channel: usize,
        destination: &mut Vec<f32>,
    ) -> bool {
        let Some(scalars) = samples_per_channel.checked_mul(2) else {
            return false;
        };
        if scalars > self.capacity_scalars {
            return false;
        }
        let mut state = self.lock();
        if state.cancelled || state.pcm.len() < scalars {
            return false;
        }
        destination.clear();
        destination.extend(state.pcm.drain(..scalars));
        drop(state);
        self.writable.notify_all();
        true
    }

    /// Return exactly `samples_per_channel` frames without blocking, or
    /// None if not enough audio is ready. This is safe on the output thread.
    /// A request larger than the configured buffer always returns None.
    pub fn try_pop_exact(&self, samples_per_channel: usize) -> Option<Vec<f32>> {
        let scalars = samples_per_channel.checked_mul(2)?;
        if scalars > self.capacity_scalars {
            return None;
        }
        let mut state = self.lock();
        if state.cancelled || state.pcm.len() < scalars {
            return None;
        }
        let popped = state.pcm.drain(..scalars).collect();
        drop(state);
        self.writable.notify_all();
        Some(popped)
    }

    /// Wait for the requested number of stereo samples, returning a shorter
    /// final frame at clean EOF. Cancellation is checked during every wait.
    /// A failed or replaced stage wakes the consumer with None.
    pub fn read_for_playback(
        &self,
        samples_per_channel: usize,
        cancellation: &crate::CancellationToken,
    ) -> Option<Vec<f32>> {
        let scalars = samples_per_channel.checked_mul(2)?;
        if scalars == 0 || scalars > self.capacity_scalars {
            return None;
        }
        let mut state = self.lock();
        while state.pcm.len() < scalars
            && !state.finished
            && !state.cancelled
            && !cancellation.is_cancelled()
        {
            let (next, _) = self
                .readable
                .wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
        if state.cancelled || cancellation.is_cancelled() || state.pcm.is_empty() {
            return None;
        }
        let count = scalars.min(state.pcm.len());
        let count = count - count % 2;
        let result = state.pcm.drain(..count).collect();
        drop(state);
        self.writable.notify_all();
        Some(result)
    }

    pub fn buffered_samples(&self) -> usize {
        self.lock().pcm.len() / 2
    }

    pub fn is_finished(&self) -> bool {
        let state = self.lock();
        state.finished
    }

    /// Mark a clean source EOF. Existing buffered frames remain readable.
    pub fn finish(&self) {
        self.lock().finished = true;
        self.writable.notify_all();
        self.readable.notify_all();
    }

    /// Invalidate the stage, discarding all buffered samples and waking its
    /// producer. An invalidated stage can never become ready again.
    pub fn cancel(&self) {
        let mut state = self.lock();
        state.cancelled = true;
        let retired = std::mem::take(&mut state.pcm);
        drop(state);
        self.writable.notify_all();
        self.readable.notify_all();
        drop(retired);
    }

    /// Release the allocated PCM ring after a promoted decoder has taken ownership and every
    /// staged sample has been consumed.
    pub(crate) fn release_storage(&self) {
        let mut state = self.lock();

        if !state.pcm.is_empty() {
            return;
        }

        let retired = std::mem::take(&mut state.pcm);
        drop(state);
        drop(retired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_buffer_is_bounded_and_nonblocking_for_the_mixer() {
        let buffer = StagedPcmBuffer::with_capacity_ms(100).unwrap();
        let mut frames = vec![0.0; 2 * 4_800];
        frames[0] = 0.25;
        buffer.push(&frames).unwrap();
        assert_eq!(buffer.buffered_samples(), 4_800);
        assert!(buffer.try_pop_exact(4_801).is_none());
        assert!(buffer.try_pop_exact(4_800).is_some());
        assert_eq!(buffer.buffered_samples(), 0);
        buffer.finish();
        assert!(buffer.is_finished());
        assert_eq!(buffer.push(&frames), Err(StageBufferError::Closed));
    }

    #[test]
    fn exact_pop_into_reuses_the_destination_and_matches_the_allocating_pop() {
        let buffer = StagedPcmBuffer::with_capacity_ms(100).unwrap();
        let frames: Vec<f32> = (0..9_600).map(|index| index as f32).collect();
        buffer.push(&frames).unwrap();

        let mut destination = Vec::with_capacity(2_000);
        let pointer = destination.as_ptr();
        assert!(buffer.try_pop_exact_into(960, &mut destination));
        assert_eq!(destination, frames[..1_920]);
        assert_eq!(
            destination.as_ptr(),
            pointer,
            "the caller's allocation is reused"
        );

        // The same data through the allocating API, for equivalence.
        assert_eq!(buffer.try_pop_exact(960).unwrap(), frames[1_920..3_840]);

        let before = destination.clone();
        assert!(!buffer.try_pop_exact_into(4_800, &mut destination));
        assert_eq!(
            destination, before,
            "a refused pop leaves the destination alone"
        );
        assert!(!buffer.try_pop_exact_into(4_801, &mut destination));
    }

    #[test]
    fn staged_buffer_storage_is_lazy_and_grows_in_bounded_chunks() {
        let buffer = StagedPcmBuffer::with_capacity_ms(2_000).unwrap();
        assert_eq!(buffer.lock().pcm.capacity(), 0);

        let frame = vec![0.0; 1_920];

        for _ in 0..12 {
            buffer.push(&frame).unwrap();
        }

        assert_eq!(buffer.lock().pcm.capacity(), 48_000);

        for _ in 12..100 {
            buffer.push(&frame).unwrap();
        }

        assert_eq!(
            buffer.lock().pcm.capacity(),
            192_000,
            "growth reaches but never exceeds the configured two-second limit"
        );
    }

    #[test]
    fn released_or_cancelled_buffers_return_their_large_allocation() {
        let buffer = StagedPcmBuffer::with_capacity_ms(2_000).unwrap();
        buffer.push(&vec![0.0; 192_000]).unwrap();
        assert_eq!(buffer.lock().pcm.capacity(), 192_000);

        buffer.cancel();
        assert_eq!(buffer.lock().pcm.capacity(), 0);
        assert_eq!(buffer.push(&[0.0, 0.0]), Err(StageBufferError::Cancelled));
    }

    #[test]
    fn drained_promoted_buffer_can_release_its_storage() {
        let buffer = StagedPcmBuffer::with_capacity_ms(2_000).unwrap();
        buffer.push(&vec![0.0; 192_000]).unwrap();
        assert_eq!(buffer.lock().pcm.capacity(), 192_000);
        assert!(buffer.try_pop_exact(96_000).is_some());

        buffer.release_storage();
        assert_eq!(buffer.lock().pcm.capacity(), 0);
    }

    #[test]
    fn cancelling_stage_releases_blocked_decoder() {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;
        let buffer = Arc::new(StagedPcmBuffer::with_capacity_ms(100).unwrap());
        buffer.push(&vec![0.0; 9_600]).unwrap();
        let pending = Arc::clone(&buffer);
        let worker = thread::spawn(move || pending.push(&[0.5, 0.5]));
        thread::sleep(Duration::from_millis(5));
        buffer.cancel();
        assert_eq!(worker.join().unwrap(), Err(StageBufferError::Cancelled));
        assert_eq!(buffer.buffered_samples(), 0);
    }

    #[test]
    #[ignore = "manual resource audit; see benchmarks/resource-audit-2-staging.md"]
    fn resource_audit_staging_realtime_stalls() {
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;
        use std::time::Instant;

        const REPEATS: u32 = 5;
        const BUFFER_COUNT: usize = 128;
        const CAPACITIES_MS: [u32; 4] = [250, 500, 1_000, 2_000];
        const OUTPUT_BLOCK_FRAMES: usize = 960;
        const OUTPUT_BLOCK: Duration = Duration::from_millis(20);
        const POST_STALL_FRAMES: usize = 48_000;

        fn fill_in_realtime_blocks(buffer: &StagedPcmBuffer, source: &[f32]) {
            for block in source.chunks(OUTPUT_BLOCK_FRAMES * 2) {
                buffer.push(block).expect("staging is open");
            }
        }

        fn sleep_until(deadline: Instant) {
            if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            }
        }

        for capacity_ms in CAPACITIES_MS {
            let capacity_frames = capacity_ms as usize * 48;
            let alignment_frames =
                (OUTPUT_BLOCK_FRAMES - capacity_frames % OUTPUT_BLOCK_FRAMES) % OUTPUT_BLOCK_FRAMES;
            let post_stall_frames = POST_STALL_FRAMES + alignment_frames;
            let source_frames = capacity_frames + post_stall_frames;
            let source: Vec<f32> = (0..source_frames)
                .flat_map(|frame| [frame as f32, frame as f32])
                .collect();
            let output_blocks = capacity_frames.div_ceil(OUTPUT_BLOCK_FRAMES)
                + post_stall_frames / OUTPUT_BLOCK_FRAMES
                + 1;

            for eager in [true, false] {
                for repeat in 0..REPEATS {
                    let started = Instant::now();
                    let buffers: Vec<_> = (0..BUFFER_COUNT)
                        .map(|_| Arc::new(StagedPcmBuffer::with_capacity_ms(capacity_ms).unwrap()))
                        .collect();
                    let bytes_at_creation = buffers
                        .iter()
                        .map(|buffer| {
                            let mut state = buffer.lock();

                            if eager {
                                state.pcm.reserve_exact(buffer.capacity_scalars);
                            }

                            state.pcm.capacity() * size_of::<f32>()
                        })
                        .sum::<usize>();

                    for buffer in &buffers {
                        fill_in_realtime_blocks(buffer, &source[..capacity_frames * 2]);
                    }

                    let bytes_at_stall = buffers
                        .iter()
                        .map(|buffer| buffer.lock().pcm.capacity() * size_of::<f32>())
                        .sum::<usize>();
                    let barrier = Arc::new(Barrier::new(BUFFER_COUNT * 2 + 1));
                    let stall = Duration::from_millis(u64::from(capacity_ms));
                    let (underflow_blocks, delivered_frames, verified_frames, continuous) =
                        thread::scope(|scope| {
                            let mut workers = Vec::with_capacity(BUFFER_COUNT);

                            for buffer in &buffers {
                                let producer_buffer = Arc::clone(buffer);
                                let producer_barrier = Arc::clone(&barrier);
                                let producer_source = &source;
                                let producer = thread::Builder::new()
                                    .stack_size(256 * 1024)
                                    .spawn_scoped(scope, move || {
                                        producer_barrier.wait();
                                        thread::sleep(stall);

                                        let mut deadline = Instant::now();

                                        for block in producer_source[capacity_frames * 2..]
                                            .chunks(OUTPUT_BLOCK_FRAMES * 2)
                                        {
                                            deadline += OUTPUT_BLOCK;
                                            sleep_until(deadline);
                                            producer_buffer
                                                .push(block)
                                                .expect("staging remains open");
                                        }
                                    })
                                    .expect("spawn bounded producer worker");

                                let consumer_buffer = Arc::clone(buffer);
                                let consumer_barrier = Arc::clone(&barrier);
                                let consumer = thread::Builder::new()
                                    .stack_size(256 * 1024)
                                    .spawn_scoped(scope, move || {
                                        consumer_barrier.wait();

                                        let mut deadline = Instant::now();
                                        let mut output =
                                            Vec::with_capacity(OUTPUT_BLOCK_FRAMES * 2);
                                        let mut expected_frame = 0;
                                        let mut delivered_frames = 0;
                                        let mut underflows = 0;
                                        let mut continuous = true;

                                        for _ in 0..output_blocks {
                                            deadline += OUTPUT_BLOCK;
                                            sleep_until(deadline);

                                            if !consumer_buffer.try_pop_exact_into(
                                                OUTPUT_BLOCK_FRAMES,
                                                &mut output,
                                            ) {
                                                underflows += 1;
                                                continue;
                                            }

                                            for (offset, pair) in
                                                output.as_chunks::<2>().0.iter().enumerate()
                                            {
                                                let expected = (expected_frame + offset) as f32;
                                                continuous &= pair == &[expected, expected];
                                            }

                                            expected_frame += OUTPUT_BLOCK_FRAMES;
                                            delivered_frames += OUTPUT_BLOCK_FRAMES;
                                        }

                                        (underflows, delivered_frames, expected_frame, continuous)
                                    })
                                    .expect("spawn bounded consumer worker");

                                workers.push((producer, consumer));
                            }

                            barrier.wait();

                            let mut underflows = 0;
                            let mut delivered = 0;
                            let mut verified = 0;
                            let mut continuous = true;

                            for (buffer, (producer, consumer)) in buffers.iter().zip(workers) {
                                producer.join().expect("producer completes");
                                let (missed, frames, mut expected_frame, mut samples_match) =
                                    consumer.join().expect("consumer completes");
                                underflows += missed;
                                delivered += frames;

                                let mut output = Vec::with_capacity(OUTPUT_BLOCK_FRAMES * 2);

                                while buffer.try_pop_exact_into(OUTPUT_BLOCK_FRAMES, &mut output) {
                                    for (offset, pair) in
                                        output.as_chunks::<2>().0.iter().enumerate()
                                    {
                                        let expected = (expected_frame + offset) as f32;
                                        samples_match &= pair == &[expected, expected];
                                    }

                                    expected_frame += OUTPUT_BLOCK_FRAMES;
                                }

                                assert_eq!(
                                    expected_frame, source_frames,
                                    "the realtime consumer and final drain account for every source frame"
                                );
                                verified += expected_frame;
                                continuous &= samples_match;
                            }

                            (underflows, delivered, verified, continuous)
                        });

                    assert!(
                        continuous,
                        "underflow did not skip or reorder media samples"
                    );
                    assert_eq!(
                        verified_frames,
                        source_frames * BUFFER_COUNT,
                        "all frames were verified across realtime output and final drain"
                    );
                    let strategy = if eager { "eager" } else { "lazy" };
                    println!(
                        "{{\"strategy\":\"{strategy}\",\"repeat\":{repeat},\"capacityMs\":{capacity_ms},\"stallMs\":{capacity_ms},\"buffers\":{BUFFER_COUNT},\"bytesAtCreation\":{bytes_at_creation},\"bytesAtStall\":{bytes_at_stall},\"underflowBlocks\":{underflow_blocks},\"deliveredFrames\":{delivered_frames},\"sampleFramesVerified\":{verified_frames},\"continuity\":{continuous},\"elapsedUs\":{}}}",
                        started.elapsed().as_micros(),
                    );
                }
            }
        }
    }
}
