use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use ffmpeg_next::ffi;
use tracing::Level;

const TARGET: &str = "remedia_core::ffmpeg";
const LOG_MESSAGE_BYTES: usize = 2_048;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_ENTRIES: usize = 256;

// On x86_64 System V targets (Linux, macOS and FreeBSD), bindgen represents
// C's `va_list` typedef as a one-element array. Function parameters adjust
// that array to a pointer, so the generated FFmpeg callbacks expect
// `*mut __va_list_tag` rather than the array typedef itself.
#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "linux", target_os = "macos", target_os = "freebsd")
))]
type FfmpegVaList = *mut ffi::__va_list_tag;
#[cfg(not(all(
    target_arch = "x86_64",
    any(target_os = "linux", target_os = "macos", target_os = "freebsd")
)))]
type FfmpegVaList = ffi::va_list;

thread_local! {
    static REMEDIA_FFMPEG_DEPTH: Cell<u32> = const { Cell::new(0) };
}

static INSTALL_CALLBACK: Once = Once::new();
static INSTALLED: OnceLock<()> = OnceLock::new();
static INITIAL_LOG_LEVEL: OnceLock<c_int> = OnceLock::new();
static RATE_LIMITER: OnceLock<Mutex<HashMap<String, RateLimitEntry>>> = OnceLock::new();

struct RateLimitEntry {
    window_started: Instant,
    suppressed: u64,
}

/// Install Remedia's one process-wide FFmpeg callback. FFmpeg exposes this
/// callback as global state, so installation is centralized and happens once.
pub(crate) fn install() {
    INSTALL_CALLBACK.call_once(|| {
        // Save the existing global verbosity before an explicitly requested
        // Remedia debug level raises the native threshold.
        let initial_level = unsafe { ffi::av_log_get_level() };
        let _ = INITIAL_LOG_LEVEL.set(initial_level);
        if let Some(level) = requested_native_log_level()
            && initial_level < level
        {
            // This is process-global FFmpeg state. It is only raised when
            // Remedia debug/trace logging is explicitly enabled.
            unsafe { ffi::av_log_set_level(level) };
        }
        // SAFETY: this callback is static for the life of the process, is
        // thread-safe, and catches panics before returning across the C ABI.
        unsafe { ffi::av_log_set_callback(Some(ffmpeg_log_callback)) };
        let _ = INSTALLED.set(());
    });
}

/// Mark FFmpeg calls made by Remedia so the process-wide callback can route
/// their messages through the current thread's Remedia tracing dispatcher.
pub(crate) fn capture() -> CaptureGuard {
    install();
    REMEDIA_FFMPEG_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
    CaptureGuard
}

pub(crate) struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        REMEDIA_FFMPEG_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

unsafe extern "C" fn ffmpeg_log_callback(
    context: *mut c_void,
    level: c_int,
    format: *const c_char,
    arguments: FfmpegVaList,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let is_remedia_call = REMEDIA_FFMPEG_DEPTH.with(|depth| depth.get() != 0);
        if !is_remedia_call {
            // FFmpeg's C API has no getter for an application's previous
            // callback. Preserve its default stderr behavior for calls outside
            // a Remedia-marked FFmpeg operation and do not exceed the level
            // configured before Remedia installed its callback.
            let initial_level = INITIAL_LOG_LEVEL.get().copied().unwrap_or(ffi::AV_LOG_INFO);
            if level <= initial_level {
                // SAFETY: FFmpeg supplied the context, format, and va_list for
                // this callback invocation; the default callback consumes them
                // synchronously, as required by FFmpeg's API.
                unsafe {
                    ffi::av_log_default_callback(context, level, format, arguments);
                }
            }
            return;
        }

        let Some(tracing_level) = tracing_level(level) else {
            return;
        };
        let enabled = match tracing_level {
            Level::ERROR => tracing::enabled!(target: TARGET, Level::ERROR),
            Level::WARN => tracing::enabled!(target: TARGET, Level::WARN),
            Level::INFO => tracing::enabled!(target: TARGET, Level::INFO),
            Level::DEBUG => tracing::enabled!(target: TARGET, Level::DEBUG),
            Level::TRACE => tracing::enabled!(target: TARGET, Level::TRACE),
        };
        if !enabled {
            return;
        }

        let message = match format_message(level, format, arguments) {
            Some(message) => redact_sensitive_text(&message),
            None => return,
        };
        if message.trim().is_empty() {
            return;
        }
        let context_name = context_name(context);
        let (emit, suppressed) = should_emit(&context_name, level, &message, Instant::now());
        if !emit {
            return;
        }

        emit_diagnostic(tracing_level, &context_name, level, &message, suppressed);
    }));

    if result.is_err() {
        // Do not unwind across FFmpeg. There is no useful structured event to
        // report when formatting itself has panicked.
    }
}

fn format_message(level: c_int, format: *const c_char, arguments: FfmpegVaList) -> Option<String> {
    if format.is_null() {
        return None;
    }
    let mut buffer = [0 as c_char; LOG_MESSAGE_BYTES];
    let mut print_prefix = 0;
    // SAFETY: FFmpeg supplied a valid NUL-terminated format and va_list for
    // this call. The stack buffer is writable for its full declared size.
    let written = unsafe {
        ffi::av_log_format_line2(
            std::ptr::null_mut(),
            level,
            format,
            arguments,
            buffer.as_mut_ptr(),
            c_int::try_from(buffer.len()).ok()?,
            &mut print_prefix,
        )
    };
    if written < 0 {
        return None;
    }
    // SAFETY: av_log_format_line2 writes a NUL-terminated message on success.
    let message = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Some(message.to_string_lossy().into_owned())
}

fn context_name(context: *mut c_void) -> String {
    if context.is_null() {
        return "ffmpeg".to_owned();
    }
    // SAFETY: FFmpeg's logging API requires `context` to be null or point to an
    // FFmpeg object whose first field is a valid AVClass pointer.
    let name = unsafe { ffi::av_default_item_name(context) };
    if name.is_null() {
        return "ffmpeg".to_owned();
    }
    // SAFETY: av_default_item_name returns a static NUL-terminated class item
    // name for a valid FFmpeg context.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .chars()
        .take(96)
        .collect()
}

fn tracing_level(level: c_int) -> Option<Level> {
    if level <= ffi::AV_LOG_ERROR {
        Some(Level::ERROR)
    } else if level <= ffi::AV_LOG_WARNING {
        Some(Level::WARN)
    } else if level <= ffi::AV_LOG_INFO {
        Some(Level::INFO)
    } else if level <= ffi::AV_LOG_DEBUG {
        Some(Level::DEBUG)
    } else if level <= ffi::AV_LOG_TRACE {
        Some(Level::TRACE)
    } else {
        None
    }
}

fn emit_diagnostic(
    level: Level,
    context: &str,
    native_level: c_int,
    message: &str,
    suppressed: u64,
) {
    match level {
        Level::ERROR => tracing::error!(
            name: "core.ffmpeg.log",
            target: TARGET,
            ffmpeg_context = context,
            ffmpeg_level = native_level,
            ffmpeg_message = message,
            suppressed_count = suppressed,
            "FFmpeg diagnostic: {message}"
        ),
        Level::WARN => tracing::warn!(
            name: "core.ffmpeg.log",
            target: TARGET,
            ffmpeg_context = context,
            ffmpeg_level = native_level,
            ffmpeg_message = message,
            suppressed_count = suppressed,
            "FFmpeg diagnostic: {message}"
        ),
        Level::INFO => tracing::info!(
            name: "core.ffmpeg.log",
            target: TARGET,
            ffmpeg_context = context,
            ffmpeg_level = native_level,
            ffmpeg_message = message,
            suppressed_count = suppressed,
            "FFmpeg diagnostic: {message}"
        ),
        Level::DEBUG => tracing::debug!(
            name: "core.ffmpeg.log",
            target: TARGET,
            ffmpeg_context = context,
            ffmpeg_level = native_level,
            ffmpeg_message = message,
            suppressed_count = suppressed,
            "FFmpeg diagnostic: {message}"
        ),
        Level::TRACE => tracing::trace!(
            name: "core.ffmpeg.log",
            target: TARGET,
            ffmpeg_context = context,
            ffmpeg_level = native_level,
            ffmpeg_message = message,
            suppressed_count = suppressed,
            "FFmpeg diagnostic: {message}"
        ),
    }
}

fn should_emit(context: &str, level: c_int, message: &str, now: Instant) -> (bool, u64) {
    let key = format!("{context}\0{level}\0{message}");
    let limiter = RATE_LIMITER.get_or_init(|| Mutex::new(HashMap::new()));
    let mut entries = limiter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = entries.get_mut(&key) {
        if now.duration_since(entry.window_started) < RATE_LIMIT_WINDOW {
            entry.suppressed = entry.suppressed.saturating_add(1);
            return (false, 0);
        }
        let suppressed = entry.suppressed;
        *entry = RateLimitEntry {
            window_started: now,
            suppressed: 0,
        };
        return (true, suppressed);
    }

    if entries.len() >= MAX_RATE_LIMIT_ENTRIES {
        entries.retain(|_, entry| now.duration_since(entry.window_started) < RATE_LIMIT_WINDOW);
        if entries.len() >= MAX_RATE_LIMIT_ENTRIES
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.window_started)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
    }
    entries.insert(
        key,
        RateLimitEntry {
            window_started: now,
            suppressed: 0,
        },
    );
    (true, 0)
}

fn requested_native_log_level() -> Option<c_int> {
    let configured_filter = std::env::var("REMEDIA_LOG").ok();
    requested_native_log_level_for(
        configured_filter.as_deref(),
        std::env::var("REMEDIA_DEBUG").as_deref() == Ok("1"),
    )
}

fn requested_native_log_level_for(filter: Option<&str>, debug_enabled: bool) -> Option<c_int> {
    if filter.is_none() && debug_enabled {
        return Some(ffi::AV_LOG_DEBUG);
    }
    let filter = filter?;
    let mut requested = None;
    for directive in filter.split([',', ';']) {
        let level = directive.rsplit('=').next()?.trim().to_ascii_lowercase();
        let target = directive.rsplit_once('=').map(|(target, _)| target.trim());
        if target.is_some_and(|target| {
            !target.is_empty()
                && target != "*"
                && target != "remedia_core"
                && target != "remedia_core::ffmpeg"
                && !target.starts_with("remedia_core::ffmpeg::")
        }) {
            continue;
        }
        match level.as_str() {
            "trace" => requested = Some(ffi::AV_LOG_TRACE),
            "debug" => requested = Some(ffi::AV_LOG_DEBUG),
            _ => {}
        }
    }
    requested
}

#[cfg(test)]
mod tests;

mod redaction;
use redaction::*;
