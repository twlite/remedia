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
    let locator = locator
        .map(|(locator, _)| CString::new(locator).map_err(|_| DecoderError::InvalidSource))
        .transpose()?;
    let mut custom_io = custom_io_for(source, cancellation.clone())?;

    let mut options = ffmpeg::Dictionary::new();
    if is_network {
        let timeout_micros = timeout.as_micros().min(i64::MAX as u128).to_string();
        options.set("rw_timeout", &timeout_micros);
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

enum CustomReaderKind {
    Memory {
        bytes: std::sync::Arc<Vec<u8>>,
        offset: usize,
    },
    Stream {
        reader: ByteStreamReader,
        cancellation: CancellationToken,
    },
}

struct CustomReader {
    kind: CustomReaderKind,
}

impl CustomReader {
    fn read(&mut self, output: &mut [u8]) -> Result<usize, ByteStreamError> {
        match &mut self.kind {
            CustomReaderKind::Memory { bytes, offset } => {
                let count = output.len().min(bytes.len().saturating_sub(*offset));
                if count > 0 {
                    output[..count].copy_from_slice(&bytes[*offset..*offset + count]);
                    *offset += count;
                }
                Ok(count)
            }
            CustomReaderKind::Stream {
                reader,
                cancellation,
            } => reader.read(output, cancellation),
        }
    }

    fn seek(&mut self, offset: i64, whence: c_int) -> Result<i64, c_int> {
        let CustomReaderKind::Memory {
            bytes,
            offset: position,
        } = &mut self.kind
        else {
            return Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::ESPIPE,
            }
            .into());
        };

        if whence & ffi::AVSEEK_SIZE != 0 {
            return i64::try_from(bytes.len()).map_err(|_| {
                ffmpeg::Error::Other {
                    errno: ffmpeg::error::EOVERFLOW,
                }
                .into()
            });
        }

        let origin = match whence & 0xffff {
            0 => 0_i128,              // SEEK_SET
            1 => *position as i128,   // SEEK_CUR
            2 => bytes.len() as i128, // SEEK_END
            _ => {
                return Err(ffmpeg::Error::Other {
                    errno: ffmpeg::error::EINVAL,
                }
                .into());
            }
        };
        let target = origin + i128::from(offset);
        if target < 0 || target > bytes.len() as i128 {
            return Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::EINVAL,
            }
            .into());
        }
        *position = target as usize;
        i64::try_from(*position).map_err(|_| {
            ffmpeg::Error::Other {
                errno: ffmpeg::error::EOVERFLOW,
            }
            .into()
        })
    }

    fn producer_failure(&self) -> Option<String> {
        match &self.kind {
            CustomReaderKind::Memory { .. } => None,
            CustomReaderKind::Stream { reader, .. } => reader.failure(),
        }
    }
}

struct CustomIo {
    context: *mut ffi::AVIOContext,
    reader: Box<CustomReader>,
}

// SAFETY: `context` is uniquely owned by this value and is never accessed by
// another thread concurrently. Moving ownership is safe; the opaque callback
// data points to the separately boxed reader, whose address remains stable.
unsafe impl Send for CustomIo {}

impl CustomIo {
    fn memory(bytes: std::sync::Arc<Vec<u8>>) -> Result<Self, DecoderError> {
        Self::new(CustomReader {
            kind: CustomReaderKind::Memory { bytes, offset: 0 },
        })
    }

    fn stream(
        source: &crate::byte_stream::ByteStreamInput,
        cancellation: CancellationToken,
    ) -> Result<Self, DecoderError> {
        let reader = source.reader();
        reader.register_cancellation(&cancellation);
        Self::new(CustomReader {
            kind: CustomReaderKind::Stream {
                reader,
                cancellation,
            },
        })
    }

    fn new(reader: CustomReader) -> Result<Self, DecoderError> {
        let mut reader = Box::new(reader);
        // FFmpeg requires this buffer to use its allocator. It may replace the
        // buffer while probing, so `Drop` frees the pointer currently in the
        // AVIOContext rather than the original pointer.
        let buffer = unsafe { ffi::av_malloc(AVIO_BUFFER_BYTES) }.cast::<u8>();
        if buffer.is_null() {
            return Err(DecoderError::OpenInput(
                "could not allocate an FFmpeg I/O buffer".to_owned(),
            ));
        }
        let opaque = (&mut *reader as *mut CustomReader).cast::<c_void>();
        let memory_source = matches!(reader.kind, CustomReaderKind::Memory { .. });
        let context = unsafe {
            ffi::avio_alloc_context(
                buffer,
                AVIO_BUFFER_BYTES as c_int,
                0,
                opaque,
                Some(read_packet_callback),
                None,
                memory_source.then_some(seek_packet_callback),
            )
        };
        if context.is_null() {
            unsafe { ffi::av_free(buffer.cast::<c_void>()) };
            return Err(DecoderError::OpenInput(
                "could not allocate an FFmpeg I/O context".to_owned(),
            ));
        }
        // SAFETY: `context` was allocated by `avio_alloc_context` above.
        unsafe {
            (*context).seekable = if memory_source {
                ffi::AVIO_SEEKABLE_NORMAL
            } else {
                0
            };
        }
        Ok(Self { context, reader })
    }

    fn as_ptr(&mut self) -> *mut ffi::AVIOContext {
        self.context
    }

    fn producer_failure(&self) -> Option<String> {
        self.reader.producer_failure()
    }
}

impl Drop for CustomIo {
    fn drop(&mut self) {
        if self.context.is_null() {
            return;
        }

        // SAFETY: this is the live AVIOContext allocation returned by
        // `avio_alloc_context`. FFmpeg may replace `buffer` during reads, so
        // free the current field before freeing the context itself.
        unsafe {
            ffi::av_freep((&mut (*self.context).buffer as *mut *mut u8).cast::<c_void>());
            ffi::avio_context_free(&mut self.context);
        }
    }
}

unsafe extern "C" fn read_packet_callback(
    opaque: *mut c_void,
    output: *mut u8,
    output_size: c_int,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() || output.is_null() || output_size <= 0 {
            return ffmpeg::Error::Other {
                errno: ffmpeg::error::EINVAL,
            }
            .into();
        }

        // SAFETY: `opaque` points to the boxed reader owned by `CustomIo` and
        // remains stable until after the AVIO context is freed. FFmpeg invokes
        // this callback synchronously on the owning decoder worker.
        let reader = unsafe { &mut *opaque.cast::<CustomReader>() };
        // SAFETY: FFmpeg supplies a writable buffer for `output_size` bytes.
        let output = unsafe { slice::from_raw_parts_mut(output, output_size as usize) };
        match reader.read(output) {
            Ok(0) => ffmpeg::Error::Eof.into(),
            Ok(count) => c_int::try_from(count).unwrap_or(c_int::MAX),
            Err(ByteStreamError::Cancelled | ByteStreamError::ConsumerClosed) => {
                ffmpeg::Error::Exit.into()
            }
            Err(ByteStreamError::ProducerFailed(_)) => ffmpeg::Error::External.into(),
            Err(_) => ffmpeg::Error::External.into(),
        }
    }))
    .unwrap_or_else(|_| ffmpeg::Error::External.into())
}

unsafe extern "C" fn seek_packet_callback(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() {
            return i64::from(c_int::from(ffmpeg::Error::Other {
                errno: ffmpeg::error::EINVAL,
            }));
        }
        // SAFETY: `opaque` points to the boxed reader owned by `CustomIo`; the
        // memory input's position is mutated only by this decoder's callbacks.
        let reader = unsafe { &mut *opaque.cast::<CustomReader>() };
        reader.seek(offset, whence).unwrap_or_else(i64::from)
    }))
    .unwrap_or_else(|_| i64::from(c_int::from(ffmpeg::Error::External)))
}

fn custom_io_for(
    source: &InputSource,
    cancellation: CancellationToken,
) -> Result<Option<CustomIo>, DecoderError> {
    match source {
        InputSource::Bytes(bytes) => CustomIo::memory(Arc::clone(bytes)).map(Some),
        InputSource::ByteStream(source) => CustomIo::stream(source, cancellation).map(Some),
        InputSource::File(_) | InputSource::HttpUrl(_) => Ok(None),
    }
}

fn source_locator(source: &InputSource) -> Result<Option<(String, bool)>, DecoderError> {
    match source {
        InputSource::File(path) => Ok(Some((path_to_string(path)?, false))),
        InputSource::HttpUrl(value) => {
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
