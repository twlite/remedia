//! Fixed, bounded executor for exclusive operations that cannot run on codec workers.
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: AtomicUsize,
    busy_ns: AtomicU64,
}

pub(crate) struct BlockingExecutor {
    inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
}

pub(crate) struct TaskHandle(Arc<(Mutex<Option<bool>>, Condvar)>);

impl TaskHandle {
    pub(crate) fn join(self) -> std::thread::Result<()> {
        let mut done = lock(&self.0.0);
        while done.is_none() {
            done = wait(&self.0.1, done);
        }
        if *done == Some(false) {
            Err(Box::new("isolated operation panicked"))
        } else {
            Ok(())
        }
    }
}

impl BlockingExecutor {
    pub(crate) fn new(workers: usize, capacity: usize, name: &str) -> std::io::Result<Self> {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            capacity,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            busy_ns: AtomicU64::new(0),
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
                            inner.active.fetch_add(1, Ordering::Relaxed);
                            let start = Instant::now();
                            let _ = catch_unwind(AssertUnwindSafe(task));
                            inner.busy_ns.fetch_add(
                                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                                Ordering::Relaxed,
                            );
                            inner.completed.fetch_add(1, Ordering::Relaxed);
                            inner.active.fetch_sub(1, Ordering::Relaxed);
                        }
                    })?,
            );
        }
        Ok(executor)
    }

    pub(crate) fn execute(&self, task: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
        let mut state = lock(&self.inner.state);
        if state.shutdown || state.queue.len() >= self.inner.capacity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "isolated operation queue is full or stopped",
            ));
        }
        state.queue.push_back(Box::new(task));
        self.inner
            .peak
            .fetch_max(state.queue.len(), Ordering::Relaxed);
        drop(state);
        self.inner.changed.notify_one();
        Ok(())
    }

    pub(crate) fn submit(
        &self,
        task: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<TaskHandle> {
        let done = Arc::new((Mutex::new(None), Condvar::new()));
        let finished = done.clone();
        self.execute(move || {
            let ok = catch_unwind(AssertUnwindSafe(task)).is_ok();
            *lock(&finished.0) = Some(ok);
            finished.1.notify_all();
        })?;
        Ok(TaskHandle(done))
    }

    pub(crate) fn queued(&self) -> usize {
        lock(&self.inner.state).queue.len()
    }

    pub(crate) fn active(&self) -> usize {
        self.inner.active.load(Ordering::Relaxed)
    }

    pub(crate) fn peak(&self) -> usize {
        self.inner.peak.load(Ordering::Relaxed)
    }

    pub(crate) fn completed(&self) -> usize {
        self.inner.completed.load(Ordering::Relaxed)
    }

    pub(crate) fn busy_ns(&self) -> u64 {
        self.inner.busy_ns.load(Ordering::Relaxed)
    }

    pub(crate) fn workers(&self) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_is_bounded_and_workers_survive_panicking_tasks() {
        let executor = BlockingExecutor::new(2, 2, "bounded-test").unwrap();
        let started = Arc::new(std::sync::Barrier::new(3));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let started = started.clone();
            let release = release.clone();
            handles.push(
                executor
                    .submit(move || {
                        started.wait();
                        let mut released = lock(&release.0);
                        while !*released {
                            released = wait(&release.1, released);
                        }
                    })
                    .unwrap(),
            );
        }
        started.wait();
        handles.push(executor.submit(|| {}).unwrap());
        handles.push(executor.submit(|| {}).unwrap());
        assert_eq!(executor.queued(), 2);
        assert!(executor.submit(|| {}).is_err());
        *lock(&release.0) = true;
        release.1.notify_all();
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(
            executor
                .submit(|| panic!("isolated panic"))
                .unwrap()
                .join()
                .is_err()
        );
        executor.submit(|| {}).unwrap().join().unwrap();
        assert_eq!(executor.peak(), 2);
    }
}
