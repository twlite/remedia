//! Fixed-size FIFO executor. Admission bounds both live tasks and readiness storage.
use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

const IDLE: usize = 0;
const QUEUED: usize = 1;
const RUNNING: usize = 2;
const NOTIFIED: usize = 3;
const DONE: usize = 4;
const TIMED: usize = 5;

type DeadlineKey = (Instant, usize);

#[derive(Default)]
struct ReadyQueue {
    jobs: VecDeque<Arc<Job>>,
    deadlines: BTreeMap<DeadlineKey, Arc<Job>>,
    shutdown: bool,
}

/// An application-owned experimental CPU pool, shared by explicitly attached sessions.
/// Dropping the last owner shuts down and joins all pool threads. Dedicated playback remains
/// the default. Only owned-byte input and built-in nonblocking outputs are eligible initially.
pub struct PlaybackScheduler {
    inner: Arc<Pool>,
    threads: Vec<JoinHandle<()>>,
}

struct Pool {
    queue: Mutex<ReadyQueue>,
    changed: Condvar,
    live: AtomicUsize,
    scheduled_live: AtomicUsize,
    capacity: usize,
    polls: AtomicUsize,
    budget: usize,
    turns: AtomicUsize,
    admitted: AtomicUsize,
    migrated: AtomicUsize,
    unsupported: AtomicUsize,
    reasons: [AtomicUsize; 5],
    rejected: AtomicUsize,
    queue_peak: AtomicUsize,
    queue_contentions: AtomicUsize,
    queue_wait_ns: std::sync::atomic::AtomicU64,
    deadline_suspensions: AtomicUsize,
    deadline_wakes: AtomicUsize,
    deadline_peak: AtomicUsize,
}

/// Instantaneous scheduler diagnostics; counts include all sessions attached to this pool.
#[derive(Clone, Copy, Debug)]
pub struct SchedulerDiagnostics {
    /// Fixed OS worker count.
    pub workers: usize,
    /// Admitted tasks, including suspended sessions.
    pub sessions: usize,
    /// Currently admitted sessions still executing on CPU workers, excluding control fallbacks.
    pub scheduled_sessions: usize,
    /// Tasks currently queued for CPU work.
    pub queued: usize,
    /// Completed bounded action invocations since pool creation.
    pub actions: usize,
    /// Maximum actions per turn. Readiness/control boundaries are checked on every action.
    pub processing_budget: usize,
    /// CPU queue dispatches, including turns that immediately suspend.
    pub turns: usize,
    /// Cumulative scheduled admissions, including already completed sessions.
    pub admitted: usize,
    /// Cumulative permanent control migrations to dedicated execution.
    pub control_fallbacks: usize,
    /// Cumulative unsupported-workload dedicated fallbacks.
    pub unsupported_fallbacks: usize,
    /// Unsupported input-source fallbacks (files, network, producer streams).
    pub input_fallbacks: usize,
    /// Outputs lacking the nonblocking scheduling contract, including realtime outputs.
    pub output_fallbacks: usize,
    /// Owned inputs with demuxers outside the memory-only whitelist.
    pub demuxer_fallbacks: usize,
    /// Sessions starting with a native filtergraph.
    pub filter_fallbacks: usize,
    /// Crossfade sessions retaining dedicated execution.
    pub crossfade_fallbacks: usize,
    /// Cumulative capacity-related dedicated fallbacks.
    pub capacity_fallbacks: usize,
    /// Maximum observed runnable queue length.
    pub queue_peak: usize,
    /// Queue mutex acquisitions which could not immediately acquire the mutex.
    pub queue_contentions: usize,
    /// Total measured nanoseconds waiting for contended queue mutex acquisitions.
    pub queue_wait_ns: u64,
    /// Current generation-specific pacing deadlines (at most one per admitted job).
    pub pending_deadlines: usize,
    /// Maximum number of pending pacing deadlines.
    pub deadline_peak: usize,
    /// Pacing suspensions, including deadlines replaced after an early control wake.
    pub deadline_suspensions: usize,
    /// Expired deadlines promoted to the runnable FIFO.
    pub deadline_wakes: usize,
}

impl PlaybackScheduler {
    /// Create a fixed pool with a positive worker count and positive live-session capacity.
    /// Thread creation failures are returned; already-created workers are shut down and joined.
    pub fn new(workers: usize, capacity: usize) -> std::io::Result<Arc<Self>> {
        Self::with_processing_budget(workers, capacity, 1)
    }

    /// Create a fixed pool with a positive maximum number of actions per scheduling turn.
    /// Each action still checks output readiness and pending controls. The budget bounds action
    /// count, not native codec execution time. A larger budget can increase fairness latency.
    pub fn with_processing_budget(
        workers: usize,
        capacity: usize,
        actions_per_turn: usize,
    ) -> std::io::Result<Arc<Self>> {
        if workers == 0 || capacity == 0 || actions_per_turn == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workers, capacity and processing budget must be positive",
            ));
        }
        let inner = Arc::new(Pool {
            queue: Mutex::new(ReadyQueue::default()),
            changed: Condvar::new(),
            live: AtomicUsize::new(0),
            scheduled_live: AtomicUsize::new(0),
            capacity,
            polls: AtomicUsize::new(0),
            budget: actions_per_turn,
            turns: AtomicUsize::new(0),
            admitted: AtomicUsize::new(0),
            migrated: AtomicUsize::new(0),
            unsupported: AtomicUsize::new(0),
            reasons: std::array::from_fn(|_| AtomicUsize::new(0)),
            rejected: AtomicUsize::new(0),
            queue_peak: AtomicUsize::new(0),
            queue_contentions: AtomicUsize::new(0),
            queue_wait_ns: std::sync::atomic::AtomicU64::new(0),
            deadline_suspensions: AtomicUsize::new(0),
            deadline_wakes: AtomicUsize::new(0),
            deadline_peak: AtomicUsize::new(0),
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
        let (queued, pending_deadlines) = {
            let queue = lock(&self.inner.queue);
            (queue.jobs.len(), queue.deadlines.len())
        };
        SchedulerDiagnostics {
            workers: self.threads.len(),
            sessions: self.inner.live.load(Ordering::Relaxed),
            scheduled_sessions: self.inner.scheduled_live.load(Ordering::Relaxed),
            queued,
            actions: self.inner.polls.load(Ordering::Relaxed),
            processing_budget: self.inner.budget,
            turns: self.inner.turns.load(Ordering::Relaxed),
            admitted: self.inner.admitted.load(Ordering::Relaxed),
            control_fallbacks: self.inner.migrated.load(Ordering::Relaxed),
            unsupported_fallbacks: self.inner.unsupported.load(Ordering::Relaxed),
            input_fallbacks: self.inner.reasons[0].load(Ordering::Relaxed),
            output_fallbacks: self.inner.reasons[1].load(Ordering::Relaxed),
            demuxer_fallbacks: self.inner.reasons[2].load(Ordering::Relaxed),
            filter_fallbacks: self.inner.reasons[3].load(Ordering::Relaxed),
            crossfade_fallbacks: self.inner.reasons[4].load(Ordering::Relaxed),
            capacity_fallbacks: self.inner.rejected.load(Ordering::Relaxed),
            queue_peak: self.inner.queue_peak.load(Ordering::Relaxed),
            queue_contentions: self.inner.queue_contentions.load(Ordering::Relaxed),
            queue_wait_ns: self.inner.queue_wait_ns.load(Ordering::Relaxed),
            pending_deadlines,
            deadline_peak: self.inner.deadline_peak.load(Ordering::Relaxed),
            deadline_suspensions: self.inner.deadline_suspensions.load(Ordering::Relaxed),
            deadline_wakes: self.inner.deadline_wakes.load(Ordering::Relaxed),
        }
    }

    pub(super) fn unsupported(&self, reason: usize) {
        self.inner.reasons[reason].fetch_add(1, Ordering::Relaxed);
        self.inner.unsupported.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn submit(
        &self,
        machine: impl FnMut() -> worker::WorkerStep + Send + 'static,
        on_error: impl Fn(std::io::Error) + Send + Sync + 'static,
    ) -> Result<Arc<Job>, Machine> {
        let machine: Machine = Box::new(machine);
        let mut live = self.inner.live.load(Ordering::Acquire);
        loop {
            if live >= self.inner.capacity {
                self.inner.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(machine);
            }
            match self.inner.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => live = current,
            }
        }
        self.inner.admitted.fetch_add(1, Ordering::Relaxed);
        self.inner.scheduled_live.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(Job {
            on_error: Box::new(on_error),
            migrated: std::sync::atomic::AtomicBool::new(false),
            cancel_wake: Mutex::new(None),
            pool: Arc::downgrade(&self.inner),
            state: AtomicUsize::new(IDLE),
            deadline: Mutex::new(None),
            machine: Mutex::new(Some(machine)),
            finished: Mutex::new(false),
            changed: Condvar::new(),
        });
        Ok(job)
    }
}

impl Drop for PlaybackScheduler {
    fn drop(&mut self) {
        lock(&self.inner.queue).shutdown = true;
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
    migrated: std::sync::atomic::AtomicBool,
    on_error: Box<dyn Fn(std::io::Error) + Send + Sync>,
    pub(super) cancel_wake: Mutex<Option<Arc<CancelWake>>>,
    pool: std::sync::Weak<Pool>,
    state: AtomicUsize,
    deadline: Mutex<Option<DeadlineKey>>,
    machine: Mutex<Option<Machine>>,
    finished: Mutex<bool>,
    changed: Condvar,
}

impl Job {
    pub(super) fn wake(self: &Arc<Self>) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                IDLE | TIMED => {
                    if self
                        .state
                        .compare_exchange(state, QUEUED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        if let Some(pool) = self.pool.upgrade() {
                            let mut queue = queue_lock(&pool);
                            if let Some(key) = lock(&self.deadline).take() {
                                queue.deadlines.remove(&key);
                            }
                            queue.jobs.push_back(self.clone());
                            pool.queue_peak
                                .fetch_max(queue.jobs.len(), Ordering::Relaxed);
                            drop(queue);
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

fn queue_lock(pool: &Pool) -> std::sync::MutexGuard<'_, ReadyQueue> {
    match pool.queue.try_lock() {
        Ok(queue) => queue,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            pool.queue_contentions.fetch_add(1, Ordering::Relaxed);
            let start = Instant::now();
            let queue = lock(&pool.queue);
            pool.queue_wait_ns.fetch_add(
                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
            queue
        }
    }
}

fn run(pool: Arc<Pool>) {
    loop {
        let job = {
            let mut queue = queue_lock(&pool);
            loop {
                let now = Instant::now();
                while queue
                    .deadlines
                    .first_key_value()
                    .is_some_and(|(key, _)| key.0 <= now)
                {
                    let (_, due) = queue.deadlines.pop_first().expect("due deadline");
                    lock(&due.deadline).take();
                    if due
                        .state
                        .compare_exchange(TIMED, QUEUED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        queue.jobs.push_back(due);
                        pool.deadline_wakes.fetch_add(1, Ordering::Relaxed);
                    }
                }
                pool.queue_peak
                    .fetch_max(queue.jobs.len(), Ordering::Relaxed);
                if queue.shutdown {
                    return;
                }
                if let Some(job) = queue.jobs.pop_front() {
                    if !queue.jobs.is_empty() {
                        pool.changed.notify_one();
                    }
                    break job;
                }
                queue = if let Some((key, _)) = queue.deadlines.first_key_value() {
                    let delay = key.0.saturating_duration_since(Instant::now());
                    pool.changed
                        .wait_timeout(queue, delay)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                } else {
                    wait(&pool.changed, queue)
                };
            }
        };
        job.state.store(RUNNING, Ordering::Release);
        let mut machine = lock(&job.machine);
        let mut actions = 0;
        let outcome = loop {
            let outcome = machine.as_mut().expect("live task")();
            actions += 1;
            if outcome != worker::WorkerStep::Runnable || actions >= pool.budget {
                break outcome;
            }
        };
        pool.polls.fetch_add(actions, Ordering::Relaxed);
        pool.turns.fetch_add(1, Ordering::Relaxed);
        if let worker::WorkerStep::Deadline(deadline) = outcome {
            let mut queue = queue_lock(&pool);
            if job
                .state
                .compare_exchange(RUNNING, TIMED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let key = (deadline, Arc::as_ptr(&job) as usize);
                let earlier = queue
                    .deadlines
                    .first_key_value()
                    .is_none_or(|(first, _)| key < *first);
                *lock(&job.deadline) = Some(key);
                queue.deadlines.insert(key, job.clone());
                pool.deadline_suspensions.fetch_add(1, Ordering::Relaxed);
                pool.deadline_peak
                    .fetch_max(queue.deadlines.len(), Ordering::Relaxed);
                if earlier {
                    pool.changed.notify_one();
                }
            } else {
                drop(queue);
                job.state.store(IDLE, Ordering::Release);
                job.wake();
            }
        } else if outcome == worker::WorkerStep::DedicatedFallback {
            pool.migrated.fetch_add(1, Ordering::Relaxed);
            pool.scheduled_live.fetch_sub(1, Ordering::Relaxed);
            job.migrated.store(true, Ordering::Release);
            let mut dedicated = machine.take().expect("migrating machine");
            drop(machine);
            let fallback_job = job.clone();
            let fallback_pool = pool.clone();
            if let Err(error) = thread::Builder::new()
                .name("remedia-control-fallback".into())
                .spawn(move || {
                    if catch_unwind(AssertUnwindSafe(&mut dedicated)).is_err() {
                        (fallback_job.on_error)(std::io::Error::other(
                            "dedicated fallback panicked",
                        ));
                    }
                    drop(dedicated);
                    complete(&fallback_job, &fallback_pool);
                })
            {
                (job.on_error)(error);
                complete(&job, &pool);
            }
        } else if matches!(
            outcome,
            worker::WorkerStep::Completed
                | worker::WorkerStep::Cancelled
                | worker::WorkerStep::Failed
        ) {
            machine.take();
            complete(&job, &pool);
        } else if outcome == worker::WorkerStep::Runnable
            || job
                .state
                .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            job.state.store(IDLE, Ordering::Release);
            job.wake();
        }
    }
}

fn complete(job: &Job, pool: &Pool) {
    if !job.migrated.load(Ordering::Acquire) {
        pool.scheduled_live.fetch_sub(1, Ordering::Relaxed);
    }
    job.state.store(DONE, Ordering::Release);
    pool.live.fetch_sub(1, Ordering::AcqRel);
    *lock(&job.finished) = true;
    job.changed.notify_all();
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
