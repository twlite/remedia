//! Fixed-size FIFO executor. Admission bounds both live tasks and readiness storage.
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

const IDLE: usize = 0;
const QUEUED: usize = 1;
const RUNNING: usize = 2;
const NOTIFIED: usize = 3;
const DONE: usize = 4;

/// An application-owned experimental CPU pool, shared by explicitly attached sessions.
/// Dropping the last owner shuts down and joins all pool threads. Dedicated playback remains
/// the default. Only owned-byte input and built-in nonblocking outputs are eligible initially.
pub struct PlaybackScheduler {
    inner: Arc<Pool>,
    threads: Vec<JoinHandle<()>>,
}

struct Pool {
    queue: Mutex<(VecDeque<Arc<Job>>, bool)>,
    changed: Condvar,
    live: AtomicUsize,
    capacity: usize,
    polls: AtomicUsize,
}

/// Instantaneous scheduler diagnostics; counts include all sessions attached to this pool.
#[derive(Clone, Copy, Debug)]
pub struct SchedulerDiagnostics {
    /// Fixed OS worker count.
    pub workers: usize,
    /// Admitted tasks, including suspended sessions.
    pub sessions: usize,
    /// Tasks currently queued for CPU work.
    pub queued: usize,
    /// Completed bounded action invocations since pool creation.
    pub actions: usize,
}

impl PlaybackScheduler {
    /// Create a fixed pool with a positive worker count and positive live-session capacity.
    /// Thread creation failures are returned; already-created workers are shut down and joined.
    pub fn new(workers: usize, capacity: usize) -> std::io::Result<Arc<Self>> {
        if workers == 0 || capacity == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workers and capacity must be positive",
            ));
        }
        let inner = Arc::new(Pool {
            queue: Mutex::new((VecDeque::new(), false)),
            changed: Condvar::new(),
            live: AtomicUsize::new(0),
            capacity,
            polls: AtomicUsize::new(0),
        });
        let mut scheduler = Self {
            inner,
            threads: Vec::new(),
        };
        for index in 0..workers {
            let pool = scheduler.inner.clone();
            scheduler.threads.push(
                thread::Builder::new()
                    .name(format!("remedia-cpu-{index}"))
                    .spawn(move || run(pool))?,
            );
        }
        Ok(Arc::new(scheduler))
    }

    /// Create a pool using available parallelism, capped at four workers, and 1024 live tasks.
    pub fn for_available_parallelism() -> std::io::Result<Arc<Self>> {
        Self::new(
            thread::available_parallelism().map_or(1, |n| n.get().min(4)),
            1024,
        )
    }

    /// Read bounded-queue and execution counters without collecting OS resources.
    pub fn diagnostics(&self) -> SchedulerDiagnostics {
        SchedulerDiagnostics {
            workers: self.threads.len(),
            sessions: self.inner.live.load(Ordering::Relaxed),
            queued: lock(&self.inner.queue).0.len(),
            actions: self.inner.polls.load(Ordering::Relaxed),
        }
    }

    pub(super) fn submit(
        &self,
        machine: impl FnMut() -> worker::WorkerStep + Send + 'static,
    ) -> Result<Arc<Job>, Machine> {
        let machine: Machine = Box::new(machine);
        if self
            .inner
            .live
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.inner.capacity).then_some(n + 1)
            })
            .is_err()
        {
            return Err(machine);
        }
        let job = Arc::new(Job {
            cancel_wake: Mutex::new(None),
            pool: Arc::downgrade(&self.inner),
            state: AtomicUsize::new(IDLE),
            machine: Mutex::new(Some(machine)),
            finished: Mutex::new(false),
            changed: Condvar::new(),
        });
        Ok(job)
    }
}

impl Drop for PlaybackScheduler {
    fn drop(&mut self) {
        lock(&self.inner.queue).1 = true;
        self.inner.changed.notify_all();
        for thread in self.threads.drain(..) {
            // Last ownership can retire on a worker after its final session is released.
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

type Machine = Box<dyn FnMut() -> worker::WorkerStep + Send>;

pub(super) struct Job {
    pub(super) cancel_wake: Mutex<Option<Arc<CancelWake>>>,
    pool: std::sync::Weak<Pool>,
    state: AtomicUsize,
    machine: Mutex<Option<Machine>>,
    finished: Mutex<bool>,
    changed: Condvar,
}

impl Job {
    pub(super) fn wake(self: &Arc<Self>) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                IDLE => {
                    if self
                        .state
                        .compare_exchange(IDLE, QUEUED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        if let Some(pool) = self.pool.upgrade() {
                            lock(&pool.queue).0.push_back(self.clone());
                            pool.changed.notify_one();
                        }
                        return;
                    }
                }
                RUNNING => {
                    if self
                        .state
                        .compare_exchange(RUNNING, NOTIFIED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    pub(super) fn join(&self) {
        let mut finished = lock(&self.finished);
        while !*finished {
            finished = wait(&self.changed, finished);
        }
    }
}

fn run(pool: Arc<Pool>) {
    loop {
        let job = {
            let mut queue = lock(&pool.queue);
            while queue.0.is_empty() && !queue.1 {
                queue = wait(&pool.changed, queue);
            }
            if queue.1 {
                return;
            }
            queue.0.pop_front().expect("nonempty queue")
        };
        job.state.store(RUNNING, Ordering::Release);
        let mut machine = lock(&job.machine);
        let outcome = machine.as_mut().expect("live task")();
        pool.polls.fetch_add(1, Ordering::Relaxed);
        if outcome == worker::WorkerStep::Terminal {
            job.state.store(DONE, Ordering::Release);
            machine.take();
            pool.live.fetch_sub(1, Ordering::AcqRel);
            *lock(&job.finished) = true;
            job.changed.notify_all();
        } else if outcome == worker::WorkerStep::Runnable {
            job.state.store(IDLE, Ordering::Release);
            job.wake();
        } else if job
            .state
            .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            job.state.store(IDLE, Ordering::Release);
            job.wake();
        }
    }
}

pub(super) struct CancelWake(pub std::sync::Weak<Job>);
impl crate::source::CancellationWake for CancelWake {
    fn wake_for_cancellation(&self) {
        if let Some(job) = self.0.upgrade() {
            job.wake();
        }
    }
}

impl std::task::Wake for CancelWake {
    fn wake(self: Arc<Self>) {
        if let Some(job) = self.0.upgrade() {
            job.wake();
        }
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(job) = self.0.upgrade() {
            job.wake();
        }
    }
}
