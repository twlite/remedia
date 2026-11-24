use super::*;
use crate::playback::blocking::BlockingExecutor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::task::Waker;

const PACKETS: usize = 8;
const BYTES: usize = 128 * 1024;

struct State {
    input: Option<OpenedInput>,
    packets: VecDeque<ffmpeg::Packet>,
    bytes: usize,
    running: bool,
    eof: bool,
    error: Option<DecoderError>,
    waker: Option<Waker>,
}

pub(crate) struct QueuedInput {
    state: Mutex<State>,
    changed: Condvar,
    executor: Arc<BlockingExecutor>,
    cancellation: CancellationToken,
    network: bool,
    seekable: AtomicBool,
}

impl QueuedInput {
    pub(super) fn new(
        input: OpenedInput,
        executor: Arc<BlockingExecutor>,
        network: bool,
        cancellation: CancellationToken,
    ) -> Arc<Self> {
        let seekable = AtomicBool::new(input.supports_random_access());
        Arc::new(Self {
            state: Mutex::new(State {
                input: Some(input),
                packets: VecDeque::new(),
                bytes: 0,
                running: false,
                eof: false,
                error: None,
                waker: None,
            }),
            changed: Condvar::new(),
            executor,
            cancellation,
            network,
            seekable,
        })
    }

    pub(crate) fn supports_random_access(&self) -> bool {
        self.seekable.load(Ordering::Acquire)
    }

    pub(crate) fn poll_packet(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<ffmpeg::Packet>, DecoderError>> {
        let mut state = lock(&self.state);
        if self.cancellation.is_cancelled() {
            return Poll::Ready(Err(DecoderError::Cancelled));
        }
        state.waker = Some(cx.waker().clone());
        let packet = state.packets.pop_front();
        if let Some(packet) = &packet {
            state.bytes -= packet.size();
        }
        if state.packets.len() <= 2 && !state.running && !state.eof && state.error.is_none() {
            state.running = true;
            let input = self.clone();
            if let Err(error) = self.executor.execute(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| input.fill())).is_err()
                {
                    let mut state = lock(&input.state);
                    state.running = false;
                    state.error = Some(DecoderError::Decode("demux read task panicked".into()));
                    let wake = state.waker.take();
                    drop(state);
                    input.changed.notify_all();
                    if let Some(wake) = wake {
                        wake.wake();
                    }
                }
            }) {
                state.running = false;
                state.error = Some(DecoderError::OpenInput(error.to_string()));
            }
        }
        if let Some(packet) = packet {
            return Poll::Ready(Ok(Some(packet)));
        }
        if let Some(error) = state.error.take() {
            return Poll::Ready(Err(error));
        }
        if state.eof {
            Poll::Ready(Ok(None))
        } else {
            Poll::Pending
        }
    }

    fn fill(&self) {
        let (mut input, slots, room) = {
            let mut state = lock(&self.state);
            (
                state.input.take().expect("exclusive demux input"),
                PACKETS - state.packets.len(),
                BYTES.saturating_sub(state.bytes),
            )
        };
        let mut packets = VecDeque::new();
        let mut bytes = 0;
        let mut eof = false;
        let mut error = None;
        for _ in 0..slots {
            match read_packet(&mut input, self.network, &self.cancellation) {
                Ok(Some(packet)) => {
                    bytes += packet.size();
                    packets.push_back(packet);
                }
                Ok(None) => {
                    eof = true;
                    break;
                }
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            }
            // One oversized packet is allowed, as on the original direct read path. Never
            // read a second packet after crossing the byte watermark.
            if bytes >= room {
                break;
            }
        }
        self.seekable
            .store(input.supports_random_access(), Ordering::Release);
        let input = if self.cancellation.is_cancelled() {
            drop(input);
            None
        } else {
            Some(input)
        };
        let mut state = lock(&self.state);
        state.input = input;
        state.packets.append(&mut packets);
        state.bytes += bytes;
        state.eof = eof;
        state.error = error;
        state.running = false;
        let wake = state.waker.take();
        drop(state);
        self.changed.notify_all();
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    pub(crate) fn wait_idle(&self) {
        let mut state = lock(&self.state);
        while state.running {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(crate) fn wait_ready(&self) {
        let mut state = lock(&self.state);
        while state.packets.is_empty() && state.running && !state.eof && state.error.is_none() {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(crate) fn seek(&self, timestamp: i64) -> Result<(), ffmpeg::Error> {
        let mut state = lock(&self.state);
        while state.running {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        // No read is in flight while the isolated control owns the machine. Discard the old
        // read-ahead only after a successful native seek; a failed seek preserves its packets.
        state
            .input
            .as_mut()
            .expect("idle demux input")
            .seek(timestamp, ..)?;
        state.packets.clear();
        state.bytes = 0;
        state.eof = false;
        state.error = None;
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
