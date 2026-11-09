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
    /// An HTTP or HTTPS URL that can be read exactly once, from the first byte
    /// to the last. See [`InputSource::sequential_http_url`].
    SequentialHttpUrl(String),
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

    /// Construct a single-pass HTTP(S) input for resources which cannot answer
    /// `Range` requests, reopen, or reconnect, such as single-use media grants.
    ///
    /// FFmpeg is told not to treat the resource as seekable, so it sends no
    /// `Range` header and never reconnects. The source is therefore a forward-only
    /// byte stream: playback can seek forward in media time by decoding and
    /// discarding frames, but cannot rewind. Use [`InputSource::http_url`] for
    /// ordinary resources; that variant keeps FFmpeg's seekable behavior.
    ///
    /// URL syntax and scheme are validated when the decoder is opened. A resource
    /// which does not return the complete body for a plain `GET` is not supported.
    pub fn sequential_http_url(url: impl Into<String>) -> Self {
        Self::SequentialHttpUrl(url.into())
    }

    /// Construct an in-memory input. The resulting source owns its bytes and
    /// can be cloned without copying the media payload.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(Arc::new(bytes.into()))
    }

    /// Create a byte-wise non-seekable input and its bounded byte producer.
    /// Playback can still seek *forward in media time* by decoding and discarding
    /// frames; backwards seeking requires a rewindable source.
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
            Self::SequentialHttpUrl(_) => "http_sequential",
            Self::Bytes(_) => "buffer",
            Self::ByteStream(_) => "byte_stream",
        }
    }

    pub(crate) fn can_seek(&self) -> bool {
        !matches!(self, Self::ByteStream(_) | Self::SequentialHttpUrl(_))
    }
}

impl PartialEq for InputSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(left), Self::File(right)) => left == right,
            (Self::HttpUrl(left), Self::HttpUrl(right)) => left == right,
            (Self::SequentialHttpUrl(left), Self::SequentialHttpUrl(right)) => left == right,
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
