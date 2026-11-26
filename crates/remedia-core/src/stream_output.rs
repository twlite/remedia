//! Bounded output shared by embedded hosts and custom transports.
use crate::{
    CancellationToken, EncodedOpusOutput, EncodedOpusPacket, MediaOutput, OpusEncoderConfig,
    OpusEncoderOutput, OpusPacketSink, OutputError, PcmFrame,
};
use std::{
    collections::VecDeque,
    future::poll_fn,
    sync::{Arc, Condvar, Mutex},
    task::{Poll, Waker},
    time::{Duration, Instant},
};

/// Raw normalized PCM or boundary-preserving Opus output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamFormat {
    S16le,
    F32le,
    Opus,
}

/// Production follows downstream demand, or the normalized output clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamPacing {
    Unpaced,
    Realtime,
}

/// One owned output chunk. PCM is interleaved little-endian 48 kHz stereo.
#[derive(Debug)]
pub struct StreamChunk {
    /// Owned payload, moved into the host runtime.
    pub data: Vec<u8>,
    /// Continuous normalized output sample position.
    pub start_sample: u64,
    /// Encoded duration (equals media duration for PCM).
    pub samples_per_channel: u32,
    /// Media duration, excluding Opus padding and pre-roll.
    pub media_samples_per_channel: u32,
}

// Readiness is a soft boundary between worker actions. A hard byte limit also bounds
// bursts from a single filter operation; unusually large bursts fail explicitly.
const READY_BYTES: usize = 64 * 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const READY_CHUNKS: usize = 256;
const MAX_CHUNKS: usize = 4096;

#[derive(Default)]
struct State {
    queue: VecDeque<StreamChunk>,
    bytes: usize,
    closed: bool,
    error: Option<String>,
    wake: bool,
    reader: Option<Waker>,
    producer: Option<Waker>,
    pacing_producer: Option<Waker>,
    clock: Option<(Instant, u64)>,
    next_sample: u64,
    paused: Option<Instant>,
}

struct Queue {
    state: Mutex<State>,
    changed: Condvar,
    pacing: StreamPacing,
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self, state: &mut State, pacing_changed: bool) {
        self.changed.notify_all();
        if let Some(waker) = state.reader.take() {
            waker.wake();
        }
        if let Some(waker) = state.producer.take() {
            waker.wake();
        }
        if pacing_changed && let Some(waker) = state.pacing_producer.take() {
            waker.wake();
        }
    }

    fn push(
        &self,
        chunk: StreamChunk,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut state = self.lock();
        if state.closed || cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }
        if chunk.data.len() > MAX_BYTES.saturating_sub(state.bytes)
            || state.queue.len() >= MAX_CHUNKS
        {
            return Err(OutputError::new(
                "stream output burst exceeds 8 MiB or 4096 chunks",
            ));
        }
        state
            .clock
            .get_or_insert((Instant::now(), chunk.start_sample));
        state.next_sample = chunk
            .start_sample
            .saturating_add(u64::from(chunk.media_samples_per_channel));
        state.bytes += chunk.data.len();
        state.queue.push_back(chunk);
        self.notify(&mut state, false);
        Ok(())
    }

    fn flush(&self) {
        let mut state = self.lock();
        state.queue.clear();
        state.bytes = 0;
        state.clock = None;
        self.notify(&mut state, true);
    }

    fn close(&self, error: Option<String>, discard: bool) {
        let mut state = self.lock();
        if state.closed {
            if discard {
                state.queue.clear();
                state.bytes = 0;
                self.notify(&mut state, true);
            }
            return;
        }
        state.closed = true;
        state.error = error;
        if discard {
            state.queue.clear();
            state.bytes = 0;
        }
        self.notify(&mut state, true);
    }
}

/// Single-consumer asynchronous queue reader. Dropping it cancels pending production.
pub struct StreamReader {
    queue: Arc<Queue>,
}

impl StreamReader {
    /// Await one owned chunk, EOF, or the playback failure. No thread is occupied.
    pub async fn next(&mut self) -> Result<Option<StreamChunk>, OutputError> {
        poll_fn(|cx| self.poll_next(cx)).await
    }

    /// Poll readiness without blocking; intended for future readiness-driven hosts.
    pub fn poll_next(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Option<StreamChunk>, OutputError>> {
        let mut state = self.queue.lock();
        if let Some(chunk) = state.queue.pop_front() {
            state.bytes -= chunk.data.len();
            self.queue.notify(&mut state, false);
            return Poll::Ready(Ok(Some(chunk)));
        }
        if let Some(error) = state.error.take() {
            return Poll::Ready(Err(OutputError::new(error)));
        }
        if state.closed {
            return Poll::Ready(Ok(None));
        }
        state.reader = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Cancel this queue only; never affects a later output generation.
    pub fn cancel(&self) {
        self.queue.close(None, true);
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct PacketQueue(Arc<Queue>);
impl OpusPacketSink for PacketQueue {
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.0.push(
            StreamChunk {
                data: packet.bytes,
                start_sample: packet.start_sample,
                samples_per_channel: packet.samples_per_channel,
                media_samples_per_channel: packet.media_samples_per_channel,
            },
            cancellation,
        )
    }
    fn flush(&self, _: Duration, _: &CancellationToken) -> Result<(), OutputError> {
        self.0.flush();
        Ok(())
    }
    fn finish(&self, _: &CancellationToken) -> Result<(), OutputError> {
        self.0.close(None, false);
        Ok(())
    }
    fn cancel(&self) {
        self.0.close(None, true);
    }
}

/// Transport-independent output on the ordinary playback pipeline.
/// The worker waits at action boundaries; no additional bridge thread is created.
pub struct StreamOutput {
    queue: Arc<Queue>,
    format: StreamFormat,
    opus: Option<OpusEncoderOutput>,
    registered: std::sync::atomic::AtomicBool,
}

impl StreamOutput {
    /// Inspect production readiness without blocking a thread. A full queue registers
    /// the single producer's waker. `Ready(Ok(None))` allows an action now;
    /// `Ready(Ok(Some(deadline)))` asks the host to schedule that absolute deadline.
    /// This does not reserve capacity: only one playback worker may produce at a time.
    /// Control wakes also permit an action so the host can process pending commands.
    pub fn poll_ready(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<Option<Instant>, OutputError>> {
        if !self
            .registered
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            cancellation.register_waker(self.queue.clone());
        }
        let mut state = self.queue.lock();
        if state.closed || cancellation.is_cancelled() {
            return Poll::Ready(Err(OutputError::Cancelled));
        }
        // A new poll retires old registrations before choosing one suspension reason.
        state.producer = None;
        state.pacing_producer = None;
        if std::mem::take(&mut state.wake) {
            return Poll::Ready(Ok(None));
        }
        if state.bytes >= READY_BYTES || state.queue.len() >= READY_CHUNKS {
            state.producer = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let deadline = if self.queue.pacing == StreamPacing::Realtime && state.paused.is_none() {
            state
                .clock
                .map(|(origin, sample)| {
                    origin
                        + Duration::from_secs_f64(
                            state.next_sample.saturating_sub(sample) as f64 / 48_000.0,
                        )
                })
                .filter(|deadline| *deadline > Instant::now())
        } else {
            None
        };
        if deadline.is_some() {
            state.pacing_producer = Some(cx.waker().clone());
        }
        Poll::Ready(Ok(deadline))
    }

    /// Create a one-shot output and reader with bounded native buffering.
    /// Encoder configuration affects PCM encoding, never source packet passthrough.
    pub fn new(
        format: StreamFormat,
        pacing: StreamPacing,
        config: OpusEncoderConfig,
    ) -> Result<(Self, StreamReader), OutputError> {
        config.validate()?;
        let queue = Arc::new(Queue {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            pacing,
        });
        let opus = if format == StreamFormat::Opus {
            Some(OpusEncoderOutput::new_with_config(
                PacketQueue(queue.clone()),
                config,
            )?)
        } else {
            None
        };
        Ok((
            Self {
                queue: queue.clone(),
                format,
                opus,
                registered: std::sync::atomic::AtomicBool::new(false),
            },
            StreamReader { queue },
        ))
    }
}

impl MediaOutput for StreamOutput {
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_nonblocking(&self) -> bool {
        true
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_deadline(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<Option<Instant>, OutputError>> {
        self.poll_ready(cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_ready(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        self.poll_ready(cx, cancellation)
            .map(|result| result.map(|_| ()))
    }

    fn wait_ready(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if !self
            .registered
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            cancellation.register_waker(self.queue.clone());
        }
        let mut state = self.queue.lock();
        loop {
            if state.closed || cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            if std::mem::take(&mut state.wake) {
                return Ok(());
            }
            if state.bytes >= READY_BYTES || state.queue.len() >= READY_CHUNKS {
                state = self
                    .queue
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }
            if self.queue.pacing == StreamPacing::Realtime
                && let Some((origin, sample)) = state.clock
            {
                let deadline = origin
                    + Duration::from_secs_f64(
                        state.next_sample.saturating_sub(sample) as f64 / 48_000.0,
                    );
                if let Some(delay) = deadline.checked_duration_since(Instant::now()) {
                    state = self
                        .queue
                        .changed
                        .wait_timeout(state, delay)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                    continue;
                }
            }
            return Ok(());
        }
    }
    fn wake(&self) {
        let mut state = self.queue.lock();
        state.wake = true;
        self.queue.notify(&mut state, true);
    }
    fn fail(&self, message: String) {
        self.queue.close(Some(message), true);
    }
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if let Some(opus) = &self.opus {
            return opus.write_frame(frame, cancellation);
        }
        let width = if self.format == StreamFormat::S16le {
            2
        } else {
            4
        };
        let mut data = Vec::with_capacity(frame.samples.len() * width);
        for sample in frame.samples {
            if width == 2 {
                data.extend_from_slice(
                    &((sample.clamp(-1.0, 1.0) * 32768.0)
                        .round()
                        .clamp(-32768.0, 32767.0) as i16)
                        .to_le_bytes(),
                );
            } else {
                data.extend_from_slice(&sample.to_le_bytes());
            }
        }
        self.queue.push(
            StreamChunk {
                data,
                start_sample: frame.start_sample,
                samples_per_channel: frame.samples_per_channel as u32,
                media_samples_per_channel: frame.samples_per_channel as u32,
            },
            cancellation,
        )
    }
    fn encoded_opus_output(&self) -> Option<&dyn EncodedOpusOutput> {
        self.opus
            .as_ref()
            .map(|opus| opus as &dyn EncodedOpusOutput)
    }
    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if let Some(opus) = &self.opus {
            opus.flush(position, cancellation)
        } else {
            self.queue.flush();
            Ok(())
        }
    }
    fn flush_samples(
        &self,
        sample: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if let Some(opus) = &self.opus {
            opus.flush_samples(sample, cancellation)
        } else {
            self.queue.flush();
            Ok(())
        }
    }
    fn filter_transition(
        &self,
        sample: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if let Some(opus) = &self.opus {
            opus.filter_transition(sample, cancellation)
        } else {
            Ok(())
        }
    }
    fn pause(&self, _: &CancellationToken) -> Result<(), OutputError> {
        self.queue.lock().paused = Some(Instant::now());
        Ok(())
    }
    fn resume(&self, _: &CancellationToken) -> Result<(), OutputError> {
        let mut state = self.queue.lock();
        if let Some(paused) = state.paused.take()
            && let Some((origin, _)) = &mut state.clock
        {
            *origin += paused.elapsed();
        }
        Ok(())
    }
    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if let Some(opus) = &self.opus {
            opus.finish(cancellation)
        } else {
            self.queue.close(None, false);
            Ok(())
        }
    }
    fn cancel(&self) {
        self.queue.close(None, true);
    }
}

// A host retains a control handle while the worker owns another reference.
impl MediaOutput for Arc<StreamOutput> {
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_deadline(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<Option<Instant>, OutputError>> {
        (**self).scheduler_poll_deadline(cx, cancellation)
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_nonblocking(&self) -> bool {
        (**self).scheduler_nonblocking()
    }

    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_ready(
        &self,
        cx: &mut std::task::Context<'_>,
        cancellation: &CancellationToken,
    ) -> Poll<Result<(), OutputError>> {
        (**self).scheduler_poll_ready(cx, cancellation)
    }

    fn wait_ready(&self, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).wait_ready(c)
    }
    fn wake(&self) {
        (**self).wake();
    }
    fn fail(&self, message: String) {
        (**self).fail(message);
    }
    fn write_frame(&self, f: PcmFrame, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).write_frame(f, c)
    }
    fn encoded_opus_output(&self) -> Option<&dyn EncodedOpusOutput> {
        (**self).encoded_opus_output()
    }
    fn flush(&self, p: Duration, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).flush(p, c)
    }
    fn flush_samples(&self, s: u64, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).flush_samples(s, c)
    }
    fn filter_transition(&self, s: u64, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).filter_transition(s, c)
    }
    fn pause(&self, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).pause(c)
    }
    fn resume(&self, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).resume(c)
    }
    fn finish(&self, c: &CancellationToken) -> Result<(), OutputError> {
        (**self).finish(c)
    }
    fn cancel(&self) {
        (**self).cancel();
    }
}

impl crate::source::CancellationWake for Queue {
    fn wake_for_cancellation(&self) {
        let mut state = self.lock();
        self.notify(&mut state, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Context;

    fn read(reader: &mut StreamReader) -> Poll<Result<Option<StreamChunk>, OutputError>> {
        reader.poll_next(&mut Context::from_waker(Waker::noop()))
    }

    fn output(format: StreamFormat) -> (StreamOutput, StreamReader) {
        StreamOutput::new(format, StreamPacing::Unpaced, OpusEncoderConfig::default()).unwrap()
    }

    #[test]
    fn pcm_endianness_clipping_and_eof() {
        for format in [StreamFormat::S16le, StreamFormat::F32le] {
            let (output, mut reader) = output(format);
            let token = CancellationToken::new();
            output
                .write_frame(
                    PcmFrame {
                        samples: vec![-1.0, 1.0, 0.5, -0.5],
                        start_sample: 0,
                        samples_per_channel: 2,
                    },
                    &token,
                )
                .unwrap();
            output.finish(&token).unwrap();
            let Poll::Ready(Ok(Some(chunk))) = read(&mut reader) else {
                panic!("missing PCM");
            };
            let expected = if format == StreamFormat::S16le {
                [-32768_i16, 32767, 16384, -16384]
                    .into_iter()
                    .flat_map(i16::to_le_bytes)
                    .collect::<Vec<_>>()
            } else {
                [-1_f32, 1.0, 0.5, -0.5]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>()
            };
            assert_eq!(chunk.data, expected);
            assert!(matches!(read(&mut reader), Poll::Ready(Ok(None))));
        }
    }

    #[test]
    fn flush_discards_native_audio_and_failure_wakes_reader() {
        let (output, mut reader) = output(StreamFormat::S16le);
        let token = CancellationToken::new();
        output
            .write_frame(
                PcmFrame {
                    samples: vec![0.0; 8],
                    start_sample: 0,
                    samples_per_channel: 4,
                },
                &token,
            )
            .unwrap();
        output.flush_samples(48000, &token).unwrap();
        assert!(read(&mut reader).is_pending());
        output.fail("decoder failed".into());
        assert!(matches!(read(&mut reader), Poll::Ready(Err(_))));
    }

    #[test]
    fn stalled_readiness_is_interruptible_and_cancellation_aware() {
        let (output, reader) = output(StreamFormat::F32le);
        let output = Arc::new(output);
        let token = CancellationToken::new();
        output
            .write_frame(
                PcmFrame {
                    samples: vec![0.0; READY_BYTES / 4],
                    start_sample: 0,
                    samples_per_channel: READY_BYTES / 8,
                },
                &token,
            )
            .unwrap();
        std::thread::scope(|scope| {
            let producer = scope.spawn(|| output.wait_ready(&token));
            output.wake();
            producer.join().unwrap().unwrap();
            let producer = scope.spawn(|| output.wait_ready(&token));
            token.cancel();
            assert!(producer.join().unwrap().is_err());
        });
        drop(reader);
    }

    #[test]
    fn passthrough_keeps_owned_packet_and_timing() {
        let (output, mut reader) = output(StreamFormat::Opus);
        let token = CancellationToken::new();
        let bytes = vec![0xf8, 0xff, 0xfe];
        let ptr = bytes.as_ptr();
        output
            .encoded_opus_output()
            .unwrap()
            .write_encoded_packet(
                EncodedOpusPacket {
                    bytes,
                    start_sample: 0,
                    samples_per_channel: 960,
                    media_samples_per_channel: 960,
                },
                &token,
            )
            .unwrap();
        let Poll::Ready(Ok(Some(chunk))) = read(&mut reader) else {
            panic!("missing packet");
        };
        assert_eq!(chunk.data.as_ptr(), ptr);
        assert_eq!(chunk.samples_per_channel, 960);
        assert_eq!(chunk.media_samples_per_channel, 960);
    }

    #[test]
    fn oversized_burst_and_abandoned_consumer_are_bounded() {
        let (output, reader) = output(StreamFormat::F32le);
        let token = CancellationToken::new();
        assert!(
            output
                .queue
                .push(
                    StreamChunk {
                        data: vec![0; MAX_BYTES + 1],
                        start_sample: 0,
                        samples_per_channel: 1,
                        media_samples_per_channel: 1
                    },
                    &token
                )
                .is_err()
        );
        drop(reader);
        assert!(output.wait_ready(&token).is_err());
    }
    #[test]
    fn draining_capacity_does_not_wake_a_pacing_deadline_but_close_does() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Wake;

        struct Counter(AtomicUsize);
        impl Wake for Counter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let (output, mut reader) = StreamOutput::new(
            StreamFormat::F32le,
            StreamPacing::Realtime,
            OpusEncoderConfig::default(),
        )
        .unwrap();
        let token = CancellationToken::new();
        output
            .write_frame(
                PcmFrame {
                    samples: vec![0.0; 1920],
                    start_sample: 0,
                    samples_per_channel: 960,
                },
                &token,
            )
            .unwrap();
        // Keep the test independent of host scheduling delays without sleeping.
        output.queue.lock().clock = Some((Instant::now() + Duration::from_secs(60), 0));
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        assert!(matches!(
            output.poll_ready(&mut Context::from_waker(&waker), &token),
            Poll::Ready(Ok(Some(_)))
        ));
        assert!(matches!(read(&mut reader), Poll::Ready(Ok(Some(_)))));
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);
        reader.cancel();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }
}
