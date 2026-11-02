use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::byte_stream::{ByteStreamError, ByteStreamInput, ByteStreamWriter};

/// An explicitly selected media input. Strings are never guessed to be paths
/// or URLs; callers choose the corresponding variant.
#[derive(Clone, Debug)]
pub enum InputSource {
    /// A local filesystem path. Unicode paths are passed to FFmpeg as UTF-8.
    File(PathBuf),
    /// An HTTP or HTTPS URL.
    HttpUrl(String),
    /// Encoded media bytes owned by Remedia.
    Bytes(Arc<Vec<u8>>),
    /// A bounded one-shot stream of encoded media bytes.
    ByteStream(ByteStreamInput),
}

impl InputSource {
    /// Construct a local file input.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    /// Construct an HTTP(S) input. URL syntax and scheme are validated when
    /// the decoder is opened.
    pub fn http_url(url: impl Into<String>) -> Self {
        Self::HttpUrl(url.into())
    }

    /// Construct an in-memory input. The resulting source owns its bytes and
    /// can be cloned without copying the media payload.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(Arc::new(bytes.into()))
    }

    /// Create a non-seekable streaming input and its bounded byte producer.
    ///
    /// Writes to the returned [`ByteStreamWriter`] block when `max_pending_bytes`
    /// are waiting for the decoder. Dropping or cancelling the decoder wakes
    /// blocked writers. The writer must call [`ByteStreamWriter::finish`] for
    /// clean EOF or [`ByteStreamWriter::fail`] to propagate a producer error.
    pub fn byte_stream(
        max_pending_bytes: usize,
    ) -> Result<(Self, ByteStreamWriter), ByteStreamError> {
        let (source, writer) = ByteStreamWriter::new_pair(max_pending_bytes)?;
        Ok((Self::ByteStream(source), writer))
    }

    /// Return a safe, low-cardinality source kind for structured diagnostics.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::HttpUrl(_) => "http",
            Self::Bytes(_) => "buffer",
            Self::ByteStream(_) => "byte_stream",
        }
    }

    pub(crate) fn can_seek(&self) -> bool {
        !matches!(self, Self::ByteStream(_))
    }
}

impl PartialEq for InputSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(left), Self::File(right)) => left == right,
            (Self::HttpUrl(left), Self::HttpUrl(right)) => left == right,
            (Self::Bytes(left), Self::Bytes(right)) => left == right,
            (Self::ByteStream(left), Self::ByteStream(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for InputSource {}

/// A thread-safe signal that can interrupt a decoder while FFmpeg is blocked
/// opening or reading a source.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<CancellationState>);

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<Weak<dyn CancellationWake>>>,
}

pub(crate) trait CancellationWake: Send + Sync {
    fn wake_for_cancellation(&self);
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationToken {
    /// Create a token that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. It cannot be reset; create a new token for a new
    /// decode operation.
    pub fn cancel(&self) {
        if self.0.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }

        let wakers = {
            let mut registered = lock(&self.0.wakers);
            let wakers = registered
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            registered.retain(|waker| waker.strong_count() > 0);
            wakers
        };
        for waker in wakers {
            waker.wake_for_cancellation();
        }
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn as_opaque_ptr(&self) -> *mut std::ffi::c_void {
        (&self.0.cancelled as *const AtomicBool).cast_mut().cast()
    }

    pub(crate) fn register_waker(&self, waker: Arc<dyn CancellationWake>) {
        let mut registered = lock(&self.0.wakers);
        registered.retain(|candidate| candidate.strong_count() > 0);
        registered.push(Arc::downgrade(&waker));
        let cancelled = self.is_cancelled();
        drop(registered);

        if cancelled {
            waker.wake_for_cancellation();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
