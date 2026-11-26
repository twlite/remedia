//! Bounded crossfade event storage and runtime-independent async notifications.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use super::{CrossfadeEvent, PlaybackSession, SessionShared};

const EVENT_CAPACITY: usize = 64;

/// Lazily growing FIFO storage. The number of retained events never exceeds the fixed bound.
#[derive(Default)]
pub(super) struct CrossfadeEventQueue {
    events: VecDeque<CrossfadeEvent>,
    dropped: u64,
}

impl CrossfadeEventQueue {
    pub(super) fn push(&mut self, event: CrossfadeEvent) {
        if self.events.len() == EVENT_CAPACITY {
            self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }

        self.events.push_back(event);
    }

    pub(super) fn take(&mut self) -> Vec<CrossfadeEvent> {
        if self.events.is_empty() && self.dropped == 0 {
            return Vec::new();
        }

        let mut result = Vec::with_capacity(self.events.len() + usize::from(self.dropped > 0));

        if self.dropped > 0 {
            result.push(CrossfadeEvent::EventsDropped {
                count: std::mem::take(&mut self.dropped),
            });
        }

        result.extend(self.events.drain(..));
        result
    }
}

/// Session-scoped revisions and async subscribers for crossfade and terminal changes.
pub(super) struct CrossfadeEventSignal {
    state: Mutex<SignalState>,
}

struct SignalState {
    revision: u64,
    next_waiter_id: u64,
    waiters: HashMap<u64, Waker>,
    disposed: bool,
}

impl Default for CrossfadeEventSignal {
    fn default() -> Self {
        Self {
            state: Mutex::new(SignalState {
                revision: 0,
                next_waiter_id: 1,
                waiters: HashMap::new(),
                disposed: false,
            }),
        }
    }
}

impl CrossfadeEventSignal {
    fn lock(&self) -> MutexGuard<'_, SignalState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn revision(&self) -> u64 {
        self.lock().revision
    }

    /// Update the revision under the registry lock, then wake outside it. Executor wakers may
    /// synchronously poll their futures, so callers must also release playback locks first.
    pub(super) fn notify(&self) {
        let waiters = {
            let mut state = self.lock();
            state.revision = state.revision.wrapping_add(1);

            std::mem::take(&mut state.waiters)
        };

        for (_, waiter) in waiters {
            waiter.wake();
        }
    }

    fn dispose(&self) {
        let waiters = {
            let mut state = self.lock();

            if state.disposed {
                return;
            }

            state.disposed = true;
            state.revision = state.revision.wrapping_add(1);

            std::mem::take(&mut state.waiters)
        };

        for (_, waiter) in waiters {
            waiter.wake();
        }
    }

    fn poll_wait(
        &self,
        requested_revision: u64,
        waiter_id: &mut Option<u64>,
        waker: &Waker,
    ) -> Poll<u64> {
        let mut state = self.lock();

        if state.disposed || state.revision != requested_revision {
            let removed_waker = waiter_id
                .take()
                .and_then(|waiter_id| state.waiters.remove(&waiter_id));
            if state.waiters.is_empty() {
                state.waiters.shrink_to_fit();
            }
            let revision = state.revision;
            drop(state);
            drop(removed_waker);

            return Poll::Ready(revision);
        }

        let id = match waiter_id {
            Some(id) => *id,
            None => {
                let id = loop {
                    let candidate = state.next_waiter_id;
                    state.next_waiter_id = state.next_waiter_id.wrapping_add(1).max(1);
                    if !state.waiters.contains_key(&candidate) {
                        break candidate;
                    }
                };
                *waiter_id = Some(id);
                id
            }
        };

        match state.waiters.get_mut(&id) {
            Some(previous) if !previous.will_wake(waker) => {
                let previous_waker = std::mem::replace(previous, waker.clone());
                drop(state);
                drop(previous_waker);
            }
            Some(_) => {}
            None => {
                state.waiters.insert(id, waker.clone());
            }
        }

        Poll::Pending
    }

    fn remove_waiter(&self, waiter_id: u64) {
        let removed_waker = {
            let mut state = self.lock();
            let removed = state.waiters.remove(&waiter_id);
            if state.waiters.is_empty() {
                state.waiters.shrink_to_fit();
            }
            removed
        };

        drop(removed_waker);
    }

    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.lock().waiters.len()
    }
}

impl Drop for SessionShared {
    fn drop(&mut self) {
        if let Some(signal) = self.crossfade_events.get() {
            signal.dispose();
        }
    }
}

impl PlaybackSession {
    /// Return the current revision for crossfade events and terminal playback changes.
    ///
    /// Read it before draining events, then pass it to
    /// [`wait_for_crossfade_events`](Self::wait_for_crossfade_events). This order closes the
    /// race between draining and registering a future.
    pub fn crossfade_events_revision(&self) -> u64 {
        self.shared
            .crossfade_events
            .get_or_init(|| Arc::new(CrossfadeEventSignal::default()))
            .revision()
    }

    /// Wait without blocking an executor thread for crossfade events or terminal playback state.
    ///
    /// The returned future owns its signal and is `Send + 'static`. Dropping a pending future
    /// unregisters its waker. If `revision` is stale, the future resolves immediately.
    pub fn wait_for_crossfade_events(
        &self,
        revision: u64,
    ) -> impl Future<Output = u64> + Send + 'static {
        CrossfadeEventsWait {
            signal: Arc::clone(
                self.shared
                    .crossfade_events
                    .get_or_init(|| Arc::new(CrossfadeEventSignal::default())),
            ),
            requested_revision: revision,
            waiter_id: None,
        }
    }
}

pub(super) struct CrossfadeEventsWait {
    signal: Arc<CrossfadeEventSignal>,
    requested_revision: u64,
    waiter_id: Option<u64>,
}

impl Future for CrossfadeEventsWait {
    type Output = u64;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        this.signal.poll_wait(
            this.requested_revision,
            &mut this.waiter_id,
            context.waker(),
        )
    }
}

impl Drop for CrossfadeEventsWait {
    fn drop(&mut self) {
        if let Some(waiter_id) = self.waiter_id.take() {
            self.signal.remove_waiter(waiter_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PlaybackState, lock};
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    struct CountWake(AtomicUsize);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn event(generation: u64) -> CrossfadeEvent {
        CrossfadeEvent::PrepareRequested {
            generation,
            remaining_ms: generation,
        }
    }

    #[test]
    fn idle_sessions_do_not_allocate_a_crossfade_signal() {
        let session = PlaybackSession::new();
        assert!(session.shared.crossfade_events.get().is_none());

        let revision = session.crossfade_events_revision();

        assert_eq!(revision, 0);
        assert!(session.shared.crossfade_events.get().is_some());
    }

    #[test]
    fn queue_allocates_lazily_and_reports_exact_loss_with_newest_events() {
        let mut queue = CrossfadeEventQueue::default();
        assert_eq!(queue.events.capacity(), 0);

        for generation in 0..(EVENT_CAPACITY as u64 + 6) {
            queue.push(event(generation));
        }

        let events = queue.take();
        assert!(matches!(
            events.first(),
            Some(CrossfadeEvent::EventsDropped { count: 6 })
        ));
        assert_eq!(events.len(), EVENT_CAPACITY + 1);
        assert!(matches!(
            events[1],
            CrossfadeEvent::PrepareRequested { generation: 6, .. }
        ));
        assert!(matches!(
            events.last(),
            Some(CrossfadeEvent::PrepareRequested { generation: 69, .. })
        ));
        assert!(queue.take().is_empty());
    }

    #[test]
    fn revision_wait_closes_notify_before_poll_and_poll_before_notify_races() {
        let signal = Arc::new(CrossfadeEventSignal::default());
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake));
        let mut context = Context::from_waker(&waker);

        let captured = signal.revision();
        let mut before_poll = CrossfadeEventsWait {
            signal: Arc::clone(&signal),
            requested_revision: captured,
            waiter_id: None,
        };
        signal.notify();
        assert!(matches!(
            Pin::new(&mut before_poll).poll(&mut context),
            Poll::Ready(1)
        ));

        let signal = Arc::new(CrossfadeEventSignal::default());
        let mut after_poll = CrossfadeEventsWait {
            signal: Arc::clone(&signal),
            requested_revision: 0,
            waiter_id: None,
        };
        assert!(matches!(
            Pin::new(&mut after_poll).poll(&mut context),
            Poll::Pending
        ));
        after_poll.signal.notify();
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            Pin::new(&mut after_poll).poll(&mut context),
            Poll::Ready(1)
        ));
    }

    #[test]
    fn dropping_a_pending_waiter_unregisters_its_waker() {
        let signal = Arc::new(CrossfadeEventSignal::default());
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake);
        let mut context = Context::from_waker(&waker);
        let revision = signal.revision();
        let mut wait = CrossfadeEventsWait {
            requested_revision: revision,
            signal: Arc::clone(&signal),
            waiter_id: None,
        };

        assert!(matches!(
            Pin::new(&mut wait).poll(&mut context),
            Poll::Pending
        ));
        assert_eq!(signal.waiter_count(), 1);

        drop(wait);
        assert_eq!(signal.waiter_count(), 0);
    }

    #[test]
    fn large_waiter_batches_release_registry_storage_after_cancellation_and_notification() {
        let signal = Arc::new(CrossfadeEventSignal::default());
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake));
        let mut context = Context::from_waker(&waker);

        for notify in [false, true] {
            let mut waits = (0..512)
                .map(|_| CrossfadeEventsWait {
                    signal: Arc::clone(&signal),
                    requested_revision: signal.revision(),
                    waiter_id: None,
                })
                .collect::<Vec<_>>();

            for wait in &mut waits {
                assert!(Pin::new(wait).poll(&mut context).is_pending());
            }
            assert_eq!(signal.waiter_count(), 512);

            if notify {
                signal.notify();
                assert_eq!(wake.0.load(Ordering::SeqCst), 512);
            }
            drop(waits);

            assert_eq!(signal.waiter_count(), 0);
            assert_eq!(signal.lock().waiters.capacity(), 0);
        }
    }

    #[test]
    fn terminal_notifications_wake_only_after_the_playback_state_lock_is_released() {
        struct StateLockProbe {
            shared: Arc<SessionShared>,
            future: Mutex<Option<Pin<Box<dyn Future<Output = u64> + Send>>>>,
            unlocked: AtomicBool,
            repolled_ready: AtomicBool,
        }

        impl Wake for StateLockProbe {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.unlocked
                    .store(self.shared.data.try_lock().is_ok(), Ordering::SeqCst);

                let waker = Waker::from(Arc::clone(self));
                let mut context = Context::from_waker(&waker);
                let mut future = self.future.lock().unwrap();
                let ready = matches!(
                    future
                        .as_mut()
                        .expect("registered wait future")
                        .as_mut()
                        .poll(&mut context),
                    Poll::Ready(1)
                );
                self.repolled_ready.store(ready, Ordering::SeqCst);
            }
        }

        let session = PlaybackSession::new();
        let revision = session.crossfade_events_revision();
        let probe = Arc::new(StateLockProbe {
            shared: Arc::clone(&session.shared),
            future: Mutex::new(Some(Box::pin(session.wait_for_crossfade_events(revision)))),
            unlocked: AtomicBool::new(false),
            repolled_ready: AtomicBool::new(false),
        });
        let waker = Waker::from(Arc::clone(&probe));
        let mut context = Context::from_waker(&waker);

        assert!(matches!(
            probe
                .future
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .as_mut()
                .poll(&mut context),
            Poll::Pending
        ));
        {
            let mut data = lock(&session.shared.data);
            data.state = PlaybackState::Completed;
            session.shared.signal_change();
        }
        session.shared.notify_completion_waiters();
        session.shared.notify_crossfade_events();

        assert!(probe.unlocked.load(Ordering::SeqCst));
        assert!(probe.repolled_ready.load(Ordering::SeqCst));
    }
}
