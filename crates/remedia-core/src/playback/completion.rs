use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use super::{PlaybackState, PlaybackWaiter, SessionShared, is_active, lock};

#[derive(Default)]
pub(super) struct CompletionWaiters {
    registry: Mutex<WaiterRegistry>,
}

#[derive(Default)]
struct WaiterRegistry {
    next_id: u64,
    waiters: HashMap<u64, Waker>,
}

impl CompletionWaiters {
    fn registry(&self) -> MutexGuard<'_, WaiterRegistry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&self, registration: &mut Option<u64>, waker: &Waker) {
        let mut registry = self.registry();
        let id = match registration {
            Some(id) => *id,
            None => {
                let id = loop {
                    let candidate = registry.next_id;
                    registry.next_id = registry.next_id.wrapping_add(1);
                    if !registry.waiters.contains_key(&candidate) {
                        break candidate;
                    }
                };
                *registration = Some(id);
                id
            }
        };

        match registry.waiters.get_mut(&id) {
            Some(previous) if !previous.will_wake(waker) => *previous = waker.clone(),
            Some(_) => {}
            None => {
                registry.waiters.insert(id, waker.clone());
            }
        }
    }

    fn unregister(&self, registration: u64) {
        let mut registry = self.registry();
        registry.waiters.remove(&registration);
        if registry.waiters.is_empty() {
            registry.waiters.shrink_to_fit();
        }
    }

    fn wake_all(&self) {
        let waiters = {
            let mut registry = self.registry();
            std::mem::take(&mut registry.waiters)
        };

        for (_, waker) in waiters {
            waker.wake();
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.registry().waiters.len()
    }
}

impl SessionShared {
    /// Wake outside the playback-state lock so executors can poll immediately.
    pub(super) fn notify_completion_waiters(&self) {
        self.completion_waiters.wake_all();
    }
}

pub(super) struct CompletionWait<'a> {
    waiter: &'a PlaybackWaiter,
    registration: Option<u64>,
}

impl<'a> CompletionWait<'a> {
    pub(super) fn new(waiter: &'a PlaybackWaiter) -> Self {
        Self {
            waiter,
            registration: None,
        }
    }
}

impl Future for CompletionWait<'_> {
    type Output = PlaybackState;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let data = lock(&this.waiter.shared.data);

        let terminal = if data.generation != this.waiter.generation {
            Some(PlaybackState::Stopped)
        } else if !is_active(data.state) {
            Some(data.state)
        } else {
            None
        };

        if let Some(state) = terminal {
            let registration = this.registration.take();
            drop(data);
            if let Some(registration) = registration {
                this.waiter
                    .shared
                    .completion_waiters
                    .unregister(registration);
            }

            return Poll::Ready(state);
        }

        // Keep the state lock while registering. A terminal publisher takes the
        // same lock before it releases the state and wakes registered futures.
        this.waiter
            .shared
            .completion_waiters
            .register(&mut this.registration, context.waker());
        drop(data);

        Poll::Pending
    }
}

impl Drop for CompletionWait<'_> {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            self.waiter
                .shared
                .completion_waiters
                .unregister(registration);
        }
    }
}
