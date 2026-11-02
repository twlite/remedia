use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// An explicitly selected media input. Strings are never guessed to be paths
/// or URLs; callers choose the corresponding variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// A local filesystem path. Unicode paths are passed to FFmpeg as UTF-8.
    File(PathBuf),
    /// An HTTP or HTTPS URL.
    HttpUrl(String),
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

    /// Return a safe, low-cardinality source kind for structured diagnostics.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::HttpUrl(_) => "http",
        }
    }
}

/// A thread-safe signal that can interrupt a decoder while FFmpeg is blocked
/// opening or reading a source.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Create a token that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. It cannot be reset; create a new token for a new
    /// decode operation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn as_opaque_ptr(&self) -> *mut std::ffi::c_void {
        Arc::as_ptr(&self.0).cast_mut().cast()
    }
}
