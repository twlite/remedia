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
    use remedia_observability::{DiagnosticValue, Observability, ObservabilityConfig, Severity};

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
