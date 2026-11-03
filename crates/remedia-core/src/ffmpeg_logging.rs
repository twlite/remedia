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

// On x86_64 System V targets (Linux and macOS), bindgen represents C's
// `va_list` typedef as a one-element array. As a function parameter, C adjusts
// that array type to a pointer, so FFmpeg's callback and formatting declarations
// expect this pointer instead. Other supported targets expose `va_list` in a
// form that already matches the generated function signatures.
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
type FfmpegVaList = *mut ffi::__va_list_tag;
#[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
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

fn redact_sensitive_text(message: &str) -> String {
    let redacted = redact_open_paths(&redact_urls(message));
    let redacted = redact_key_values(&redacted);
    redacted.chars().take(LOG_MESSAGE_BYTES - 1).collect()
}

fn redact_open_paths(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len().min(LOG_MESSAGE_BYTES));
    let mut copied_through = 0;
    for marker in ["Opening '", "Opening \""] {
        let mut scan_from = 0;
        while let Some(relative_start) = message[scan_from..].find(marker) {
            let marker_start = scan_from + relative_start;
            let path_start = marker_start + marker.len();
            let quote = if marker.ends_with('\'') { '\'' } else { '"' };
            let Some(relative_end) = message[path_start..].find(quote) else {
                break;
            };
            let path_end = path_start + relative_end;
            if marker_start >= copied_through {
                redacted.push_str(&message[copied_through..path_start]);
                redacted.push_str("[REDACTED_PATH]");
                copied_through = path_end;
            }
            scan_from = path_end + quote.len_utf8();
        }
    }
    redacted.push_str(&message[copied_through..]);
    redacted
}

fn redact_urls(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len().min(LOG_MESSAGE_BYTES));
    let bytes = message.as_bytes();
    let mut copied_through = 0;
    let mut scan_from = 0;
    while let Some(relative_marker) = message[scan_from..].find("://") {
        let marker = scan_from + relative_marker;
        let mut scheme_start = marker;
        while scheme_start > 0 && is_scheme_byte(bytes[scheme_start - 1]) {
            scheme_start -= 1;
        }
        if scheme_start == marker || !bytes[scheme_start].is_ascii_alphabetic() {
            scan_from = marker + 3;
            continue;
        }

        let body_start = marker + 3;
        let end = message[body_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                is_url_terminator(character).then_some(body_start + offset)
            })
            .unwrap_or(message.len());
        redacted.push_str(&message[copied_through..scheme_start]);
        redacted.push_str(&message[scheme_start..body_start]);
        redacted.push_str("[REDACTED]");
        copied_through = end;
        scan_from = end.max(body_start);
    }
    redacted.push_str(&message[copied_through..]);
    redacted
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
}

fn is_url_terminator(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '\'' | '"' | '<' | '>' | ')' | ']' | '}' | ',' | ';'
        )
}

fn redact_key_values(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(eq) = rest.find('=') {
        let key_start = rest[..eq]
            .rfind(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .map_or(0, |index| index + 1);
        let key = &rest[key_start..eq];
        if !is_sensitive_key(key) {
            output.push_str(&rest[..eq + 1]);
            rest = &rest[eq + 1..];
            continue;
        }
        let value_end = rest[eq + 1..]
            .find(|character: char| character.is_whitespace() || character == '&')
            .map_or(rest.len(), |offset| eq + 1 + offset);
        output.push_str(&rest[..key_start]);
        output.push_str(key);
        output.push_str("=[REDACTED]");
        rest = &rest[value_end..];
    }
    output.push_str(rest);
    output
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "credential",
        "sessionid",
    ]
    .iter()
    .any(|sensitive| normalized.contains(sensitive))
        || normalized == "key"
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
mod tests {
    use super::*;

    #[test]
    fn log_levels_map_native_severity_to_tracing() {
        assert_eq!(tracing_level(ffi::AV_LOG_ERROR), Some(Level::ERROR));
        assert_eq!(tracing_level(ffi::AV_LOG_WARNING), Some(Level::WARN));
        assert_eq!(tracing_level(ffi::AV_LOG_INFO), Some(Level::INFO));
        assert_eq!(tracing_level(ffi::AV_LOG_DEBUG), Some(Level::DEBUG));
        assert_eq!(tracing_level(ffi::AV_LOG_TRACE), Some(Level::TRACE));
    }

    #[test]
    fn redacts_urls_and_sensitive_key_values_from_native_messages() {
        let message = redact_sensitive_text(
            "open https://user:pass@example.test/audio?token=secret token=another-secret srt://media-user:pass@stream.test/live; Opening '/private/audio.mp3' for reading",
        );
        assert!(!message.contains("user:pass"));
        assert!(!message.contains("secret"));
        assert!(!message.contains("/private/audio.mp3"));
        assert!(message.contains("https://[REDACTED]"));
        assert!(message.contains("srt://[REDACTED]"));
        assert!(message.contains("[REDACTED_PATH]"));
    }

    #[test]
    fn repeated_native_messages_are_rate_limited_and_report_suppression() {
        let now = Instant::now();
        let context = format!("test-{}", next_rate_limit_test_key());
        assert_eq!(
            should_emit(&context, ffi::AV_LOG_WARNING, "duplicate", now),
            (true, 0)
        );
        assert_eq!(
            should_emit(&context, ffi::AV_LOG_WARNING, "duplicate", now),
            (false, 0)
        );
        assert_eq!(
            should_emit(
                &context,
                ffi::AV_LOG_WARNING,
                "duplicate",
                now + RATE_LIMIT_WINDOW
            ),
            (true, 1)
        );
    }

    #[test]
    fn process_wide_callback_installation_is_idempotent() {
        install();
        assert!(INSTALLED.get().is_some());
        // SAFETY: reading FFmpeg's process-wide log threshold has no pointer
        // arguments or ownership requirements.
        let level = unsafe { ffi::av_log_get_level() };
        for _ in 0..4 {
            install();
        }
        // SAFETY: same process-global scalar getter as above.
        assert_eq!(unsafe { ffi::av_log_get_level() }, level);
    }

    #[test]
    fn native_verbosity_obeys_debug_and_component_filter_precedence() {
        assert_eq!(requested_native_log_level_for(None, false), None);
        assert_eq!(
            requested_native_log_level_for(None, true),
            Some(ffi::AV_LOG_DEBUG)
        );
        assert_eq!(
            requested_native_log_level_for(Some("info"), true),
            None,
            "an explicit filter takes precedence over REMEDIA_DEBUG"
        );
        assert_eq!(
            requested_native_log_level_for(Some("remedia_core::ffmpeg=trace"), false),
            Some(ffi::AV_LOG_TRACE)
        );
        assert_eq!(
            requested_native_log_level_for(Some("remedia_core::playback=trace"), false),
            None,
            "a filter for another component should not raise FFmpeg verbosity"
        );
    }

    #[test]
    fn callback_routes_by_active_instance_and_maps_native_warning_severity() {
        use remedia_observability::{
            DiagnosticValue, Observability, ObservabilityConfig, Severity,
        };

        let make_observability = || {
            Observability::new(
                ObservabilityConfig::default()
                    .with_filter("remedia_core::ffmpeg=trace")
                    .expect("valid FFmpeg log filter")
                    .with_console_enabled(false),
            )
            .expect("create local diagnostic context")
        };
        let first = make_observability();
        let second = make_observability();
        let first_diagnostics = first.diagnostics();
        let second_diagnostics = second.diagnostics();

        emit_test_log(&first.handle(), "callback-route-first");
        emit_test_log(&second.handle(), "callback-route-second");

        for (diagnostics, expected_message) in [
            (&first_diagnostics, "callback-route-first"),
            (&second_diagnostics, "callback-route-second"),
        ] {
            let event = diagnostics
                .recv_timeout(Duration::from_secs(1))
                .expect("native FFmpeg message reaches this instance's diagnostics");
            assert_eq!(event.event, "core.ffmpeg.log");
            assert_eq!(event.target, TARGET);
            assert_eq!(event.severity, Severity::Warn);
            assert!(matches!(
                event.fields.get("ffmpeg_message"),
                Some(DiagnosticValue::String(message)) if message.contains(expected_message)
            ));
        }
    }

    fn emit_test_log(handle: &remedia_observability::ObservabilityHandle, message: &str) {
        let message = std::ffi::CString::new(message).expect("test message has no NUL");
        crate::with_dispatch(handle, || {
            let _capture = capture();
            let format = b"%s\n\0";
            // SAFETY: FFmpeg is initialized by `capture`; `format` is a static
            // NUL-terminated printf format containing one `%s`, and `message`
            // remains alive for the synchronous variadic call.
            unsafe {
                ffi::av_log(
                    std::ptr::null_mut(),
                    ffi::AV_LOG_WARNING,
                    format.as_ptr().cast(),
                    message.as_ptr(),
                );
            }
        });
    }

    fn next_rate_limit_test_key() -> u64 {
        static NEXT: OnceLock<Mutex<u64>> = OnceLock::new();
        let mut next = NEXT
            .get_or_init(|| Mutex::new(0))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        *next
    }
}
