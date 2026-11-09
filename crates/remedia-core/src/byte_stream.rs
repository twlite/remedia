use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

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
/// Cloning it refers to the same one-shot byte stream.
#[derive(Clone)]
pub struct ByteStreamInput {
    inner: Arc<QueueInner>,
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

/// A bounded producer for a [`ByteStreamInput`]. Writes block when the pending
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
}

struct QueueInner {
    capacity_bytes: usize,
    writer: Mutex<()>,
    state: Mutex<QueueState>,
    changed: Condvar,
}

struct QueueState {
    chunks: VecDeque<Vec<u8>>,
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

impl ByteStreamInput {
    pub(crate) fn reader(&self) -> ByteStreamReader {
        ByteStreamReader {
            inner: Arc::clone(&self.inner),
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
        });
        Ok((
            ByteStreamInput {
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
                .push_back(bytes[offset..offset + count].to_vec());
            state.pending_bytes += count;
            offset += count;
            self.inner.changed.notify_all();
        }
        Ok(())
    }

    /// Write a native-owned chunk. A chunk that fits the queue is moved into it
    /// without another copy; larger chunks are split to respect the byte cap.
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
                .push_back(bytes[offset..offset + count].to_vec());
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
    }

    fn cancel(&self) {
        let mut state = lock(&self.state);
        state.producer = ProducerState::Cancelled;
        state.consumer_closed = true;
        state.chunks.clear();
        state.front_offset = 0;
        state.pending_bytes = 0;
        self.changed.notify_all();
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
    fn cancellation_wakes_blocked_writers_and_releases_queued_bytes() {
        let (input, writer) = ByteStreamWriter::new_pair(4).unwrap();
        writer.write(b"full").unwrap();
        let blocked = writer.clone();
        let worker = thread::spawn(move || blocked.write(b"next"));
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
}
