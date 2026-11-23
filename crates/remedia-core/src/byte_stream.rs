use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use bytes::Bytes;
use thiserror::Error;

use crate::source::{CancellationToken, CancellationWake};

/// Error returned when producing bytes for a streaming media input.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ByteStreamError {
    /// A byte stream must have room for at least one pending byte.
    #[error("byte stream capacity must be greater than zero")]
    InvalidCapacity,
    /// The decoder cancelled the input or the producer explicitly cancelled it.
    #[error("byte stream was cancelled")]
    Cancelled,
    /// The decoder has stopped reading this input.
    #[error("byte stream has no active reader")]
    ConsumerClosed,
    /// The producer has already finished or failed.
    #[error("byte stream producer has already terminated")]
    Terminated,
    /// The producer explicitly failed with this message.
    #[error("byte stream producer failed: {0}")]
    ProducerFailed(String),
}

/// The source side of a bounded stream passed to [`InputSource::byte_stream`].
/// Cloning it refers to the same one-shot byte stream. Dropping every clone (and every reader
/// opened from them) without ever reading closes the consumer side, so a producer blocked in
/// [`ByteStreamWriter::write`] wakes with [`ByteStreamError::ConsumerClosed`].
#[derive(Clone)]
pub struct ByteStreamInput {
    inner: Arc<QueueInner>,
    release: Arc<ConsumerRelease>,
}

/// Closes the consumer side once the last input or reader handle is gone.
struct ConsumerRelease(Arc<QueueInner>);

impl Drop for ConsumerRelease {
    fn drop(&mut self) {
        self.0.close_consumer();
    }
}

impl fmt::Debug for ByteStreamInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ByteStreamInput")
            .field("capacity_bytes", &self.inner.capacity_bytes)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ByteStreamInput {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for ByteStreamInput {}

/// A bounded producer for a [`crate::InputSource::byte_stream`] input. Writes block when the pending
/// byte limit is reached. Cancellation and reader shutdown wake blocked calls.
#[derive(Clone)]
pub struct ByteStreamWriter {
    inner: Arc<QueueInner>,
}

impl fmt::Debug for ByteStreamWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ByteStreamWriter")
            .field("capacity_bytes", &self.inner.capacity_bytes)
            .field("pending_bytes", &self.pending_bytes())
            .finish()
    }
}

pub(crate) struct ByteStreamReader {
    inner: Arc<QueueInner>,
    /// Keeps the consumer open for as long as a reader exists, even if every input is dropped.
    _release: Arc<ConsumerRelease>,
}

struct QueueInner {
    capacity_bytes: usize,
    writer: Mutex<()>,
    state: Mutex<QueueState>,
    changed: Condvar,
    close_waiters: Mutex<Vec<Weak<AsyncCloseWake>>>,
}

struct QueueState {
    chunks: VecDeque<Bytes>,
    front_offset: usize,
    pending_bytes: usize,
    waiting_readers: usize,
    waiting_writers: usize,
    producer: ProducerState,
    consumer_closed: bool,
}

enum ProducerState {
    Open,
    Finished,
    Failed(String),
    Cancelled,
}

#[derive(Default)]
struct AsyncCloseWake(Mutex<Option<std::task::Waker>>);

impl AsyncCloseWake {
    fn wake(&self) {
        if let Some(waker) = lock(&self.0).take() {
            waker.wake();
        }
    }
}

impl ByteStreamInput {
    pub(crate) fn reader(&self) -> ByteStreamReader {
        ByteStreamReader {
            inner: Arc::clone(&self.inner),
            _release: Arc::clone(&self.release),
        }
    }
}

impl ByteStreamWriter {
    pub(crate) fn new_pair(
        capacity_bytes: usize,
    ) -> Result<(ByteStreamInput, Self), ByteStreamError> {
        if capacity_bytes == 0 {
            return Err(ByteStreamError::InvalidCapacity);
        }

        let inner = Arc::new(QueueInner {
            capacity_bytes,
            writer: Mutex::new(()),
            state: Mutex::new(QueueState {
                chunks: VecDeque::new(),
                front_offset: 0,
                pending_bytes: 0,
                waiting_readers: 0,
                waiting_writers: 0,
                producer: ProducerState::Open,
                consumer_closed: false,
            }),
            changed: Condvar::new(),
            close_waiters: Mutex::new(Vec::new()),
        });
        Ok((
            ByteStreamInput {
                release: Arc::new(ConsumerRelease(Arc::clone(&inner))),
                inner: Arc::clone(&inner),
            },
            Self { inner },
        ))
    }

    /// Write a chunk, waiting until the reader has room. The queue never holds
    /// more than its configured pending byte capacity; large chunks are split.
    pub fn write(&self, bytes: &[u8]) -> Result<(), ByteStreamError> {
        let _writer = lock(&self.inner.writer);
        if bytes.is_empty() {
            ensure_writable(&lock(&self.inner.state))?;
            return Ok(());
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let state = lock(&self.inner.state);
            let mut state = self.inner.wait_until_writable(state, 1)?;
            let available = self.inner.capacity_bytes - state.pending_bytes;
            let count = available.min(bytes.len() - offset);
            state
                .chunks
                .push_back(Bytes::copy_from_slice(&bytes[offset..offset + count]));
            state.pending_bytes += count;
            offset += count;
            self.inner.changed.notify_all();
        }
        Ok(())
    }

    /// Write a native-owned chunk. A chunk that fits the queue is moved into it
    /// without another copy; larger chunks are split to respect the pending-byte cap.
    ///
    /// The cap counts `Vec::len()`, not `Vec::capacity()`. A vector that fits the queue and has
    /// excess capacity keeps that allocation until its queued bytes are consumed.
    pub fn write_owned(&self, bytes: Vec<u8>) -> Result<(), ByteStreamError> {
        let _writer = lock(&self.inner.writer);
        if bytes.is_empty() {
            let state = lock(&self.inner.state);
            ensure_writable(&state)?;
            return Ok(());
        }

        if bytes.len() <= self.inner.capacity_bytes {
            let state = lock(&self.inner.state);
            let mut state = self.inner.wait_until_writable(state, bytes.len())?;
            state.pending_bytes += bytes.len();
            state.chunks.push_back(Bytes::from(bytes));
            self.inner.changed.notify_all();
            return Ok(());
        }

        let mut offset = 0;
        while offset < bytes.len() {
            let state = lock(&self.inner.state);
            let mut state = self.inner.wait_until_writable(state, 1)?;
            let count = (self.inner.capacity_bytes - state.pending_bytes).min(bytes.len() - offset);
            state
                .chunks
                .push_back(Bytes::copy_from_slice(&bytes[offset..offset + count]));
            state.pending_bytes += count;
            offset += count;
            self.inner.changed.notify_all();
        }
        Ok(())
    }

    /// Queue a `bytes::Bytes` chunk without an ingress copy when its visible length fits the queue.
    ///
    /// A fitting chunk retains its shared backing allocation until the decoder consumes it. A
    /// `Bytes` slice can therefore keep a much larger allocation alive than its visible length;
    /// the pending-byte limit does not bound that backing allocation. If a small slice comes from
    /// a large allocation, copy it into a right-sized `Bytes` before calling this method. For
    /// example, use `Bytes::copy_from_slice(&slice)` when retaining the slice's parent would cost
    /// more than copying it. Chunks larger than the queue capacity are copied into bounded pieces.
    /// As with other write methods, this blocks while the queue lacks room and returns when the
    /// whole input has been accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use bytes::Bytes;
    /// use remedia_core::InputSource;
    ///
    /// let (_source, writer) = InputSource::byte_stream(64 * 1024)?;
    /// writer.write_bytes(Bytes::from_static(b"encoded media"))?;
    /// writer.finish()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ByteStreamError::ConsumerClosed`] if the decoder stopped reading,
    /// [`ByteStreamError::Cancelled`] if the stream was cancelled, or the existing terminal
    /// producer error if the stream was already finished or failed.
    pub fn write_bytes(&self, bytes: Bytes) -> Result<(), ByteStreamError> {
        let _writer = lock(&self.inner.writer);
        if bytes.is_empty() {
            let state = lock(&self.inner.state);
            ensure_writable(&state)?;
            return Ok(());
        }

        if bytes.len() <= self.inner.capacity_bytes {
            let state = lock(&self.inner.state);
            let mut state = self.inner.wait_until_writable(state, bytes.len())?;
            state.pending_bytes += bytes.len();
            state.chunks.push_back(bytes);
            self.inner.changed.notify_all();
            return Ok(());
        }

        let mut offset = 0;
        while offset < bytes.len() {
            let state = lock(&self.inner.state);
            let mut state = self.inner.wait_until_writable(state, 1)?;
            let count = (self.inner.capacity_bytes - state.pending_bytes).min(bytes.len() - offset);
            state
                .chunks
                .push_back(Bytes::copy_from_slice(&bytes[offset..offset + count]));
            state.pending_bytes += count;
            offset += count;
            self.inner.changed.notify_all();
        }
        Ok(())
    }

    /// Signal that no more bytes will be written. Repeated calls are harmless.
    pub fn finish(&self) -> Result<(), ByteStreamError> {
        let _writer = lock(&self.inner.writer);
        let mut state = lock(&self.inner.state);
        match &state.producer {
            ProducerState::Open => state.producer = ProducerState::Finished,
            ProducerState::Finished => return Ok(()),
            ProducerState::Failed(message) => {
                return Err(ByteStreamError::ProducerFailed(message.clone()));
            }
            ProducerState::Cancelled => return Err(ByteStreamError::Cancelled),
        }
        self.inner.changed.notify_all();
        drop(state);
        self.inner.wake_close_waiters();
        Ok(())
    }

    /// Fail the input. Any queued bytes are delivered first, then decoding
    /// returns this message as a typed input failure.
    pub fn fail(&self, message: impl Into<String>) -> Result<(), ByteStreamError> {
        let message = message.into();
        let mut state = lock(&self.inner.state);
        match &state.producer {
            ProducerState::Open => state.producer = ProducerState::Failed(message),
            ProducerState::Finished => return Err(ByteStreamError::Terminated),
            ProducerState::Failed(message) => {
                return Err(ByteStreamError::ProducerFailed(message.clone()));
            }
            ProducerState::Cancelled => return Err(ByteStreamError::Cancelled),
        }
        self.inner.changed.notify_all();
        drop(state);
        self.inner.wake_close_waiters();
        Ok(())
    }

    /// Cancel the source, clear queued bytes, and wake blocked readers/writers.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Whether further writes can no longer reach a decoder: the consumer stopped
    /// reading, or the stream was cancelled, finished or failed. Producers which wait
    /// on external events can poll this to release upstream resources promptly.
    pub fn is_closed(&self) -> bool {
        let state = lock(&self.inner.state);

        state.consumer_closed || !matches!(state.producer, ProducerState::Open)
    }

    /// Wait asynchronously for the consumer to close, or for the producer to finish,
    /// fail, or be cancelled. No periodic wakeups or dedicated thread are needed.
    /// Dropped futures do not keep registered waiters alive.
    pub async fn closed_async(&self) {
        if self.is_closed() {
            return;
        }
        let waiter = Arc::new(AsyncCloseWake::default());
        {
            let mut listeners = lock(&self.inner.close_waiters);
            listeners.retain(|listener| listener.strong_count() > 0);
            listeners.push(Arc::downgrade(&waiter));
        }
        std::future::poll_fn(|context| {
            *lock(&waiter.0) = Some(context.waker().clone());
            if self.is_closed() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }

    /// Bytes currently queued for the decoder. This is useful for diagnostics
    /// and tests; it never includes bytes held by an upstream producer.
    pub fn pending_bytes(&self) -> usize {
        lock(&self.inner.state).pending_bytes
    }

    /// Maximum number of bytes this queue may hold pending consumption.
    pub fn capacity_bytes(&self) -> usize {
        self.inner.capacity_bytes
    }
}

impl ByteStreamReader {
    pub(crate) fn register_cancellation(&self, cancellation: &CancellationToken) {
        let wake: Arc<dyn CancellationWake> = Arc::clone(&self.inner) as Arc<_>;
        cancellation.register_waker(wake);
    }

    pub(crate) fn read(
        &self,
        output: &mut [u8],
        cancellation: &CancellationToken,
    ) -> Result<usize, ByteStreamError> {
        if output.is_empty() {
            return Ok(0);
        }

        let mut state = lock(&self.inner.state);
        loop {
            if cancellation.is_cancelled() {
                state.consumer_closed = true;
                state.chunks.clear();
                state.front_offset = 0;
                state.pending_bytes = 0;
                self.inner.changed.notify_all();
                drop(state);
                self.inner.wake_close_waiters();
                return Err(ByteStreamError::Cancelled);
            }
            if state.consumer_closed {
                return Err(ByteStreamError::ConsumerClosed);
            }
            if state.pending_bytes > 0 {
                let count = output.len().min(state.pending_bytes);
                let mut copied = 0;
                while copied < count {
                    let front_offset = state.front_offset;
                    let (take, chunk_consumed) = {
                        let chunk = state.chunks.front().expect("queued byte count is valid");
                        let take = (chunk.len() - front_offset).min(count - copied);
                        output[copied..copied + take]
                            .copy_from_slice(&chunk[front_offset..front_offset + take]);
                        (take, front_offset + take == chunk.len())
                    };
                    copied += take;
                    state.front_offset += take;
                    if chunk_consumed {
                        state.chunks.pop_front();
                        state.front_offset = 0;
                    }
                }
                state.pending_bytes -= copied;
                self.inner.changed.notify_all();
                return Ok(copied);
            }

            match &state.producer {
                ProducerState::Open => {
                    state.waiting_readers += 1;
                    self.inner.changed.notify_all();
                    state = self
                        .inner
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.waiting_readers -= 1;
                }
                ProducerState::Finished => return Ok(0),
                ProducerState::Failed(message) => {
                    return Err(ByteStreamError::ProducerFailed(message.clone()));
                }
                ProducerState::Cancelled => return Err(ByteStreamError::Cancelled),
            }
        }
    }

    pub(crate) fn failure(&self) -> Option<String> {
        match &lock(&self.inner.state).producer {
            ProducerState::Failed(message) => Some(message.clone()),
            _ => None,
        }
    }
}

impl Drop for ByteStreamReader {
    fn drop(&mut self) {
        self.inner.close_consumer();
    }
}

impl QueueInner {
    fn wait_until_writable<'a>(
        &'a self,
        mut state: MutexGuard<'a, QueueState>,
        required: usize,
    ) -> Result<MutexGuard<'a, QueueState>, ByteStreamError> {
        loop {
            ensure_writable(&state)?;
            if self.capacity_bytes - state.pending_bytes >= required {
                return Ok(state);
            }
            state = self.wait_until_changed(state);
        }
    }

    fn wait_until_changed<'a>(
        &'a self,
        mut state: MutexGuard<'a, QueueState>,
    ) -> MutexGuard<'a, QueueState> {
        state.waiting_writers += 1;
        self.changed.notify_all();
        state = self
            .changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiting_writers -= 1;
        state
    }

    fn close_consumer(&self) {
        let mut state = lock(&self.state);
        state.consumer_closed = true;
        state.chunks.clear();
        state.front_offset = 0;
        state.pending_bytes = 0;
        self.changed.notify_all();
        drop(state);
        self.wake_close_waiters();
    }

    fn cancel(&self) {
        let mut state = lock(&self.state);
        state.producer = ProducerState::Cancelled;
        state.consumer_closed = true;
        state.chunks.clear();
        state.front_offset = 0;
        state.pending_bytes = 0;
        self.changed.notify_all();
        drop(state);
        self.wake_close_waiters();
    }

    fn wake_close_waiters(&self) {
        let waiters = {
            let mut listeners = lock(&self.close_waiters);
            let live = listeners.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
            listeners.retain(|listener| listener.strong_count() > 0);
            live
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

impl CancellationWake for QueueInner {
    fn wake_for_cancellation(&self) {
        // Taking the same mutex as readers prevents a lost notification between
        // their cancellation check and Condvar wait.
        let _state = lock(&self.state);
        self.changed.notify_all();
    }
}

fn ensure_writable(state: &QueueState) -> Result<(), ByteStreamError> {
    match &state.producer {
        ProducerState::Open if state.consumer_closed => Err(ByteStreamError::ConsumerClosed),
        ProducerState::Open => Ok(()),
        ProducerState::Finished => Err(ByteStreamError::Terminated),
        ProducerState::Failed(message) => Err(ByteStreamError::ProducerFailed(message.clone())),
        ProducerState::Cancelled => Err(ByteStreamError::Cancelled),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn wait_for_state(inner: &QueueInner, predicate: impl Fn(&QueueState) -> bool) {
        let mut state = lock(&inner.state);
        while !predicate(&state) {
            state = inner
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    #[test]
    fn pending_bytes_are_bounded_and_writes_apply_backpressure() {
        let (input, writer) = ByteStreamWriter::new_pair(5).unwrap();
        let reader = input.reader();
        let write = writer.clone();
        let worker = thread::spawn(move || write.write(b"abcdefghij"));
        let cancellation = CancellationToken::new();
        reader.register_cancellation(&cancellation);
        let mut bytes = [0; 3];
        let mut output = Vec::new();

        while output.len() < 10 {
            assert!(writer.pending_bytes() <= writer.capacity_bytes());
            let count = reader.read(&mut bytes, &cancellation).unwrap();
            output.extend_from_slice(&bytes[..count]);
        }

        worker.join().unwrap().unwrap();
        assert_eq!(output, b"abcdefghij");
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn fitting_slices_retain_their_full_shared_backing_until_read() {
        let (input, writer) = ByteStreamWriter::new_pair(8).unwrap();
        let mut backing = vec![0x7f; 1024 * 1024];
        backing[2..6].copy_from_slice(b"data");
        let bytes = Bytes::from(backing).slice(2..6);
        let expected_pointer = bytes.as_ptr();
        writer.write_bytes(bytes).unwrap();

        let state = lock(&writer.inner.state);
        assert_eq!(state.pending_bytes, 4);
        assert_eq!(state.chunks.front().unwrap().as_ptr(), expected_pointer);
        drop(state);

        let reader = input.reader();
        let mut output = [0; 4];
        assert_eq!(reader.read(&mut output, &CancellationToken::new()), Ok(4));
        assert_eq!(&output, b"data");
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn producer_failure_delivers_queued_bytes_then_surfaces_the_error() {
        let (input, writer) = ByteStreamWriter::new_pair(8).unwrap();
        writer.write_bytes(Bytes::from_static(b"partial")).unwrap();
        writer.fail("truncated response").unwrap();

        let reader = input.reader();
        let mut output = [0; 8];
        assert_eq!(reader.read(&mut output, &CancellationToken::new()), Ok(7));
        assert_eq!(&output[..7], b"partial");
        assert_eq!(
            reader.read(&mut output, &CancellationToken::new()),
            Err(ByteStreamError::ProducerFailed(
                "truncated response".to_owned()
            ))
        );
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn oversized_shared_bytes_are_copied_into_bounded_queue_pieces() {
        let (input, writer) = ByteStreamWriter::new_pair(4).unwrap();
        let bytes = Bytes::from_static(b"abcdefghij");
        let source_pointer = bytes.as_ptr();
        let producer = writer.clone();
        let worker = thread::spawn(move || producer.write_bytes(bytes));
        let reader = input.reader();
        let cancellation = CancellationToken::new();
        let mut output = Vec::new();

        wait_for_state(&writer.inner, |state| state.pending_bytes > 0);
        let state = lock(&writer.inner.state);
        let first_chunk = state.chunks.front().unwrap();
        assert_ne!(first_chunk.as_ptr(), source_pointer);
        assert!(first_chunk.len() <= 4);
        drop(state);

        while output.len() < 10 {
            let state = lock(&writer.inner.state);
            assert!(state.pending_bytes <= 4);
            drop(state);

            let mut buffer = [0; 3];
            let count = reader.read(&mut buffer, &cancellation).unwrap();
            output.extend_from_slice(&buffer[..count]);
        }

        worker.join().unwrap().unwrap();
        assert_eq!(output, b"abcdefghij");
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn cancellation_wakes_blocked_writers_and_releases_queued_bytes() {
        let (input, writer) = ByteStreamWriter::new_pair(4).unwrap();
        writer.write(b"full").unwrap();
        let blocked = writer.clone();
        let worker = thread::spawn(move || blocked.write_bytes(Bytes::from_static(b"next")));
        wait_for_state(&writer.inner, |state| state.waiting_writers == 1);

        writer.cancel();

        assert_eq!(worker.join().unwrap(), Err(ByteStreamError::Cancelled));
        assert_eq!(writer.pending_bytes(), 0);
        drop(input);
    }

    #[test]
    fn cancellation_wakes_blocked_readers_without_polling() {
        let (input, _writer) = ByteStreamWriter::new_pair(4).unwrap();
        let reader = input.reader();
        let cancellation = CancellationToken::new();
        reader.register_cancellation(&cancellation);
        let decode_token = cancellation.clone();
        let worker = thread::spawn(move || {
            let mut output = [0; 4];
            reader.read(&mut output, &decode_token)
        });
        wait_for_state(&input.inner, |state| state.waiting_readers == 1);
        cancellation.cancel();
        assert_eq!(worker.join().unwrap(), Err(ByteStreamError::Cancelled));
    }

    #[test]
    fn cancellation_wakes_blocked_readers_across_repeated_streams() {
        for _ in 0..1_024 {
            let (input, _writer) = ByteStreamWriter::new_pair(4).unwrap();
            let reader = input.reader();
            let cancellation = CancellationToken::new();
            reader.register_cancellation(&cancellation);
            let decode_token = cancellation.clone();
            let worker = thread::spawn(move || {
                let mut output = [0; 4];
                reader.read(&mut output, &decode_token)
            });

            wait_for_state(&input.inner, |state| state.waiting_readers == 1);
            cancellation.cancel();

            assert_eq!(worker.join().unwrap(), Err(ByteStreamError::Cancelled));
            drop(input);
        }
    }

    #[test]
    fn dropping_every_input_without_reading_wakes_a_blocked_writer() {
        let (input, writer) = ByteStreamWriter::new_pair(4).unwrap();
        writer.write(b"full").unwrap();
        let blocked = writer.clone();
        let worker = thread::spawn(move || blocked.write_owned(b"next".to_vec()));
        wait_for_state(&writer.inner, |state| state.waiting_writers == 1);
        let clone = input.clone();

        drop(input);
        assert!(
            !writer.is_closed(),
            "a remaining clone keeps the input open"
        );
        drop(clone);

        assert_eq!(worker.join().unwrap(), Err(ByteStreamError::ConsumerClosed));
        assert!(writer.is_closed());
        assert_eq!(writer.pending_bytes(), 0);
    }

    #[test]
    fn a_live_reader_keeps_the_stream_open_after_every_input_is_dropped() {
        let (input, writer) = ByteStreamWriter::new_pair(8).unwrap();
        let reader = input.reader();
        drop(input);
        writer.write(b"abc").unwrap();
        let cancellation = CancellationToken::new();
        let mut bytes = [0; 8];

        assert!(!writer.is_closed());
        assert_eq!(reader.read(&mut bytes, &cancellation).unwrap(), 3);
        drop(reader);
        assert!(writer.is_closed());
    }

    #[test]
    fn blocked_shared_producers_release_after_cancel_or_reader_drop_repeatedly() {
        for cycle in 0..2_048 {
            let (input, writer) = ByteStreamWriter::new_pair(4).unwrap();
            let reader = input.reader();
            let producer_writer = writer.clone();
            let producer = thread::spawn(move || {
                producer_writer
                    .write_bytes(Bytes::from(vec![b'f', b'u', b'l', b'l']))
                    .unwrap();
                producer_writer.write_bytes(Bytes::from(vec![b'n', b'e', b'x', b't']))
            });

            wait_for_state(&writer.inner, |state| state.waiting_writers == 1);
            assert_eq!(writer.pending_bytes(), writer.capacity_bytes());

            let expected_error = if cycle % 2 == 0 {
                writer.cancel();
                drop(reader);
                ByteStreamError::Cancelled
            } else {
                drop(reader);
                ByteStreamError::ConsumerClosed
            };

            drop(input);

            assert_eq!(producer.join().unwrap(), Err(expected_error));
            assert_eq!(writer.pending_bytes(), 0);
            assert!(writer.is_closed());
        }
    }
}
