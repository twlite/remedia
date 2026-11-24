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
    dispatch: tracing::Dispatch,
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
            dispatch: tracing::dispatcher::get_default(Clone::clone),
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
        state.waker = None;
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
            // Register only an actual input wait. Read-ahead completion must not interrupt
            // an unrelated output-capacity or pacing suspension.
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn fill(&self) {
        tracing::dispatcher::with_default(&self.dispatch, || {
            let _capture = crate::ffmpeg_logging::capture();
            self.fill_inner();
        });
    }

    fn fill_inner(&self) {
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
            .ok_or(ffmpeg::Error::InvalidData)?
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn read_ahead_completion_only_wakes_an_empty_queue_waiter() {
        let token = CancellationToken::new();
        let source = InputSource::file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ffmpeg-opus-silent-lead-long.ogg"
        ));
        let input = input::open(&source, Duration::from_secs(5), token.clone()).unwrap();
        let executor = Arc::new(BlockingExecutor::new(1, 8, "demux-wake-test").unwrap());
        let queue = QueuedInput::new(input, executor.clone(), false, token);
        let count = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(queue.poll_packet(&mut cx).is_pending());
        queue.wait_idle();
        executor.submit(|| {}).unwrap().join().unwrap();
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
        for _ in 0..6 {
            assert!(matches!(
                queue.poll_packet(&mut cx),
                Poll::Ready(Ok(Some(_)))
            ));
        }
        queue.wait_idle();
        executor.submit(|| {}).unwrap().join().unwrap();
        assert_eq!(
            count.0.load(Ordering::Relaxed),
            1,
            "read-ahead must not wake output pacing"
        );
    }
}
