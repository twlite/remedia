use std::ffi::{CString, c_int, c_void};
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::byte_stream::{ByteStreamError, ByteStreamReader};
use crate::error::{DecoderError, ffmpeg_error};
use crate::source::{CancellationToken, InputSource};

const AVIO_BUFFER_BYTES: usize = 32 * 1024;
static FFMPEG_INITIALIZATION: OnceLock<Result<(), String>> = OnceLock::new();

pub(crate) struct OpenedInput {
    context: Option<ffmpeg::format::context::Input>,
    custom_io: Option<CustomIo>,
    seekable: bool,
    // The interrupt callback receives a pointer into this token. Keep the Arc
    // allocation alive until the native format context and custom IO are closed.
    _cancellation: CancellationToken,
}

impl Deref for OpenedInput {
    type Target = ffmpeg::format::context::Input;

    fn deref(&self) -> &Self::Target {
        self.context.as_ref().expect("input context is open")
    }
}

impl DerefMut for OpenedInput {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.context.as_mut().expect("input context is open")
    }
}

impl OpenedInput {
    pub(crate) const fn is_seekable(&self) -> bool {
        self.seekable
    }

    pub(crate) fn producer_failure(&self) -> Option<String> {
        self.custom_io.as_ref().and_then(CustomIo::producer_failure)
    }
}

impl Drop for OpenedInput {
    fn drop(&mut self) {
        // The format context may read from `pb` while closing. Release it before
        // freeing the custom AVIO context and its opaque callback state.
        drop(self.context.take());
        drop(self.custom_io.take());
    }
}

pub(crate) fn initialize() -> Result<(), DecoderError> {
    FFMPEG_INITIALIZATION
        .get_or_init(|| {
            crate::ffmpeg_logging::install();
            ffmpeg::init().map_err(|error| error.to_string())?;
            ffmpeg::format::network::init();
            Ok(())
        })
        .clone()
        .map_err(DecoderError::Initialization)
}

pub(crate) fn open(
    source: &InputSource,
    timeout: Duration,
    cancellation: CancellationToken,
) -> Result<OpenedInput, DecoderError> {
    let _log_capture = crate::ffmpeg_logging::capture();
    initialize()?;

    if cancellation.is_cancelled() {
        return Err(DecoderError::Cancelled);
    }
    if timeout.is_zero() {
        return Err(DecoderError::InvalidOptions(
            "I/O timeout must be greater than zero",
        ));
    }

    let locator = source_locator(source)?;
    let is_network = locator.as_ref().is_some_and(|(_, network)| *network);
    let sequential_http = matches!(source, InputSource::SequentialHttpUrl(_));
    let locator = locator
        .map(|(locator, _)| CString::new(locator).map_err(|_| DecoderError::InvalidSource))
        .transpose()?;
    let mut custom_io = custom_io_for(source, cancellation.clone())?;

    let mut options = ffmpeg::Dictionary::new();
    if is_network {
        let timeout_micros = timeout.as_micros().min(i64::MAX as u128).to_string();
        options.set("rw_timeout", &timeout_micros);
        // FFmpeg 9's default HTTP reader may report EIO after the terminating
        // chunk of an unknown-length response. Persistent-request mode keeps
        // the chunk-end state available to subsequent reads, which then see EOF.
        options.set("multiple_requests", "1");
    }
    if sequential_http {
        // A forward-only resource cannot answer `Range: bytes=0-` (FFmpeg sends it
        // by default) and cannot be reopened. Disable seeking, which also stops the
        // HTTP protocol from probing or issuing ranged requests, and keep automatic
        // reconnection off so an interrupted body is reported instead of replayed
        // from a different offset. Only HTTP(S) transports are permitted, including
        // for redirects.
        options.set("seekable", "0");
        options.set("reconnect", "0");
        options.set("reconnect_at_eof", "0");
        options.set("reconnect_streamed", "0");
        options.set("protocol_whitelist", "http,https,tcp,tls,crypto");
    }

    // Allocate the format context before installing the custom AVIO pointer.
    // AVFMT_FLAG_CUSTOM_IO tells libavformat that `pb` is borrowed and must not
    // be closed when the format context is released.
    let mut input = unsafe { ffi::avformat_alloc_context() };
    if input.is_null() {
        return Err(DecoderError::OpenInput(
            "could not allocate an FFmpeg format context".to_owned(),
        ));
    }
    if let Some(custom_io) = custom_io.as_mut() {
        // SAFETY: the custom AVIO context remains owned by `custom_io` until
        // after the format context is closed in every success/error path.
        unsafe {
            (*input).pb = custom_io.as_ptr();
            (*input).flags |= ffi::AVFMT_FLAG_CUSTOM_IO;
        }
    }

    // SAFETY: `cancellation` keeps this Arc allocation alive through input
    // probing, packet reads, and context destruction. The callback only reads
    // its AtomicBool and never retains the pointer.
    unsafe {
        (*input).interrupt_callback = ffi::AVIOInterruptCB {
            callback: Some(interrupt_callback),
            opaque: cancellation.as_opaque_ptr(),
        };
    }

    // The native context owns options it consumes and returns unrecognized
    // options through this pointer. Re-wrap those remaining entries for drop.
    let mut native_options = unsafe { options.disown() };
    let open_result = unsafe {
        ffi::avformat_open_input(
            &mut input,
            locator
                .as_ref()
                .map_or(ptr::null(), |locator| locator.as_ptr()),
            ptr::null_mut(),
            &mut native_options,
        )
    };
    drop(unsafe { ffmpeg::Dictionary::own(native_options) });

    if open_result < 0 {
        if !input.is_null() {
            unsafe { ffi::avformat_close_input(&mut input) };
        }
        if let Some(message) = custom_io.as_ref().and_then(CustomIo::producer_failure) {
            return Err(DecoderError::InputFailure(message));
        }
        return Err(ffmpeg_error(
            ffmpeg::Error::from(open_result),
            "opening media input",
            cancellation.is_cancelled(),
            is_network,
        ));
    }

    let probe_result = unsafe { ffi::avformat_find_stream_info(input, ptr::null_mut()) };
    if probe_result < 0 {
        unsafe { ffi::avformat_close_input(&mut input) };
        if let Some(message) = custom_io.as_ref().and_then(CustomIo::producer_failure) {
            return Err(DecoderError::InputFailure(message));
        }
        return Err(ffmpeg_error(
            ffmpeg::Error::from(probe_result),
            "probing media streams",
            cancellation.is_cancelled(),
            is_network,
        ));
    }

    if cancellation.is_cancelled() {
        unsafe { ffi::avformat_close_input(&mut input) };
        return Err(DecoderError::Cancelled);
    }

    // SAFETY: a successful `avformat_open_input` and `avformat_find_stream_info`
    // leave a valid input context. `OpenedInput` closes it exactly once.
    let context = unsafe { ffmpeg::format::context::Input::wrap(input) };
    Ok(OpenedInput {
        context: Some(context),
        custom_io,
        seekable: source.can_seek(),
        _cancellation: cancellation,
    })
}

unsafe extern "C" fn interrupt_callback(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }

    // SAFETY: `opaque` is the live AtomicBool installed by `open`; its owner is
    // retained in `OpenedInput` until after the format context closes.
    let cancelled = unsafe { &*opaque.cast::<std::sync::atomic::AtomicBool>() };
    i32::from(cancelled.load(Ordering::Acquire))
}

fn source_locator(source: &InputSource) -> Result<Option<(String, bool)>, DecoderError> {
    match source {
        InputSource::File(path) => Ok(Some((path_to_string(path)?, false))),
        InputSource::HttpUrl(value) | InputSource::SequentialHttpUrl(value) => {
            let url = url::Url::parse(value)
                .map_err(|error| DecoderError::InvalidUrl(error.to_string()))?;
            if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                return Err(DecoderError::UnsupportedUrlScheme);
            }
            Ok(Some((url.to_string(), true)))
        }
        InputSource::Bytes(_) | InputSource::ByteStream(_) => Ok(None),
    }
}

fn path_to_string(path: &Path) -> Result<String, DecoderError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(DecoderError::NonUnicodePath)
}

mod custom_io;
use custom_io::*;
