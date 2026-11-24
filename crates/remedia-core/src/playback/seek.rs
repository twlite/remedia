//! Seek requests: the outcome a caller learns, and the ticket that decides who wins a race
//! between applying a seek and cancelling it.

use super::*;
use std::sync::atomic::{AtomicU8, Ordering};

/// Where a seek ended up.
///
/// A seek asks for a position, but a fallback seek lands on the requested position plus the
/// source time that elapsed while its replacement was prepared. The caller needs the position
/// that was actually applied to keep its own clock and progress display honest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeekOutcome {
    /// The position the caller asked for, normalized to the 48 kHz sample grid.
    pub requested: Duration,
    /// The source-timeline position the audio continues from after the seek.
    pub applied: Duration,
    /// A replacement decoder took over (fallback seeking). The output timeline then continues
    /// without a reset: only the source position jumps. An ordinary seek resets both.
    pub replaced_source: bool,
}

const PENDING: u8 = 0;
const COMMITTED: u8 = 1;
const CANCELLED: u8 = 2;

/// Decides, exactly once, whether a seek is applied or cancelled.
///
/// The worker commits the ticket immediately before it changes anything audible; a cancellation
/// that arrives first wins and the seek never happens, while one that arrives later is told it
/// was too late. So a caller that cancels (a timed-out request, an aborted signal) knows from
/// the answer whether the seek can still take effect.
#[derive(Debug, Default)]
pub(super) struct SeekTicket(AtomicU8);

impl SeekTicket {
    /// Claim the seek for application. `false` if it was cancelled first.
    pub(super) fn commit(&self) -> bool {
        self.0
            .compare_exchange(PENDING, COMMITTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            || self.0.load(Ordering::Acquire) == COMMITTED
    }

    /// Return a claim the worker made but did not use, so the seek can still be cancelled.
    pub(super) fn uncommit(&self) {
        let _ = self
            .0
            .compare_exchange(COMMITTED, PENDING, Ordering::AcqRel, Ordering::Acquire);
    }

    pub(super) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == CANCELLED
    }
}

/// Cancels one submitted seek. See [`PendingSeek::canceller`].
#[derive(Clone)]
pub struct SeekCanceller {
    pub(super) ticket: Arc<SeekTicket>,
    pub(super) shared: Arc<SessionShared>,
}

impl SeekCanceller {
    /// Cancel the seek if it has not taken effect yet.
    ///
    /// Returns `true` when the seek is guaranteed never to be applied: it was still queued or
    /// still preparing its replacement, and its waiting call fails with
    /// [`PlaybackError::SeekCancelled`]. Returns `false` when the seek was already applied or is
    /// being applied, in which case it takes effect and its result is the caller's to observe.
    /// The source that was playing is never interrupted by a cancellation.
    pub fn cancel(&self) -> bool {
        let cancelled = self.ticket.cancel();
        if cancelled {
            // A worker that is paused or blocked in a read notices at its next check.
            let mut data = lock(&self.shared.data);
            data.fallback_signal = true;
            self.shared.signal_change();
        }

        cancelled
    }
}
