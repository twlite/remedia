use std::ffi::{CString, c_int, c_void};
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::error::{DecoderError, ffmpeg_error};
use crate::source::{CancellationToken, InputSource};

static FFMPEG_INITIALIZATION: OnceLock<Result<(), String>> = OnceLock::new();

pub(crate) struct OpenedInput {
    context: Option<ffmpeg::format::context::Input>,
    // The interrupt callback receives a pointer into this token. Keep the Arc
    // allocation alive until the native format context has closed.
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

impl Drop for OpenedInput {
    fn drop(&mut self) {
        // Close the context before its callback's opaque pointer can be freed.
        drop(self.context.take());
    }
}

pub(crate) fn initialize() -> Result<(), DecoderError> {
    FFMPEG_INITIALIZATION
        .get_or_init(|| {
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
    initialize()?;

    if cancellation.is_cancelled() {
        return Err(DecoderError::Cancelled);
    }
    if timeout.is_zero() {
        return Err(DecoderError::InvalidOptions(
            "I/O timeout must be greater than zero",
        ));
    }

    let (locator, is_network) = source_locator(source)?;
    let locator = CString::new(locator).map_err(|_| DecoderError::InvalidSource)?;
    let mut options = ffmpeg::Dictionary::new();
    if is_network {
        let timeout_micros = timeout.as_micros().min(i64::MAX as u128).to_string();
        options.set("rw_timeout", &timeout_micros);
    }

    let mut input = unsafe { ffi::avformat_alloc_context() };
    if input.is_null() {
        return Err(DecoderError::OpenInput(
            "could not allocate an FFmpeg format context".to_owned(),
        ));
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
            locator.as_ptr(),
            ptr::null_mut(),
            &mut native_options,
        )
    };
    drop(unsafe { ffmpeg::Dictionary::own(native_options) });

    if open_result < 0 {
        if !input.is_null() {
            unsafe { ffi::avformat_close_input(&mut input) };
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
        _cancellation: cancellation,
    })
}

unsafe extern "C" fn interrupt_callback(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }

    // SAFETY: `opaque` is the live Arc allocation installed by `open`; its
    // owner is retained in `OpenedInput` until after the FFmpeg context closes.
    let cancelled = unsafe { &*opaque.cast::<std::sync::atomic::AtomicBool>() };
    i32::from(cancelled.load(Ordering::Acquire))
}

fn source_locator(source: &InputSource) -> Result<(String, bool), DecoderError> {
    match source {
        InputSource::File(path) => Ok((path_to_string(path)?, false)),
        InputSource::HttpUrl(value) => {
            let url = url::Url::parse(value)
                .map_err(|error| DecoderError::InvalidUrl(error.to_string()))?;
            if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                return Err(DecoderError::UnsupportedUrlScheme);
            }
            Ok((url.to_string(), true))
        }
    }
}

fn path_to_string(path: &Path) -> Result<String, DecoderError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(DecoderError::NonUnicodePath)
}
