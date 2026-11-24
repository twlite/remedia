//! Fixed, bounded executor for exclusive operations that cannot run on codec workers.
use super::*;
use std::collections::VecDeque;

type Task = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct State {
    queue: VecDeque<Task>,
    shutdown: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
}

pub(super) struct BlockingExecutor {
    inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
}

impl BlockingExecutor {
    pub(super) fn new(workers: usize, capacity: usize, name: &str) -> std::io::Result<Self> {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            capacity,
        });
        let mut executor = Self {
            inner,
            threads: Vec::new(),
        };
        for index in 0..workers {
            let inner = executor.inner.clone();
            executor.threads.push(
                thread::Builder::new()
                    .name(format!("remedia-{name}-{index}"))
                    .spawn(move || {
                        loop {
                            let task = {
                                let mut state = lock(&inner.state);
                                loop {
                                    if let Some(task) = state.queue.pop_front() {
                                        break task;
                                    }
                                    if state.shutdown {
                                        return;
                                    }
                                    state = wait(&inner.changed, state);
                                }
                            };
                            task();
                        }
                    })?,
            );
        }
        Ok(executor)
    }

    pub(super) fn submit(&self, task: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
        let mut state = lock(&self.inner.state);
        if state.shutdown || state.queue.len() >= self.inner.capacity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "isolated operation queue is full or stopped",
            ));
        }
        state.queue.push_back(Box::new(move || {
            let _ = catch_unwind(AssertUnwindSafe(task));
        }));
        drop(state);
        self.inner.changed.notify_one();
        Ok(())
    }

    pub(super) fn queued(&self) -> usize {
        lock(&self.inner.state).queue.len()
    }

    pub(super) fn workers(&self) -> usize {
        self.threads.len()
    }
}

impl Drop for BlockingExecutor {
    fn drop(&mut self) {
        lock(&self.inner.state).shutdown = true;
        self.inner.changed.notify_all();
        for thread in self.threads.drain(..) {
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}
