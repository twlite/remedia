use std::time::Duration;

use remedia_observability::{DiagnosticEvent, DiagnosticValue, Severity};

use crate::diagnostics::{diagnostic_value, to_native_event};
use crate::util::{duration_from_millis, parse_snowflake};

#[test]
fn parses_decimal_snowflakes_without_number_coercion() {
    assert_eq!(
        parse_snowflake("guildId", "18446744073709551615").unwrap(),
        u64::MAX
    );
    assert!(parse_snowflake("guildId", "0").is_err());
    assert!(parse_snowflake("guildId", "12.0").is_err());
    assert!(parse_snowflake("guildId", " 12").is_err());
    assert!(parse_snowflake("guildId", "18446744073709551616").is_err());
}

#[test]
fn converts_millisecond_positions_to_typed_durations() {
    assert_eq!(
        duration_from_millis(1250.5).unwrap(),
        Duration::from_micros(1_250_500)
    );
    assert!(duration_from_millis(f64::NAN).is_err());
    assert!(duration_from_millis(-1.0).is_err());
}

#[test]
fn structured_debug_events_keep_typed_fields_and_correlation_strings() {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("attempt".to_owned(), DiagnosticValue::U64(4));
    fields.insert("is_retry".to_owned(), DiagnosticValue::Bool(true));
    fields.insert(
        "error".to_owned(),
        DiagnosticValue::String("offline".to_owned()),
    );
    let event = to_native_event(DiagnosticEvent {
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        severity: Severity::Warn,
        target: "remedia_core::playback".to_owned(),
        event: "core.playback.failed".to_owned(),
        message: Some("could not decode".to_owned()),
        fields,
        playback_id: Some(9),
        voice_connection_id: Some(u64::MAX),
    });
    assert_eq!(event.timestamp, 0.0);
    assert_eq!(event.severity, "warn");
    assert_eq!(event.playback_id.as_deref(), Some("9"));
    assert_eq!(
        event.voice_connection_id.as_deref(),
        Some("18446744073709551615")
    );
    assert_eq!(event.fields["attempt"].kind, "integer");
    assert_eq!(event.fields["attempt"].string_value.as_deref(), Some("4"));
    assert_eq!(event.fields["is_retry"].kind, "boolean");
    assert_eq!(event.fields["is_retry"].bool_value, Some(true));
    assert_eq!(event.fields["error"].kind, "string");
    assert_eq!(
        event.fields["error"].string_value.as_deref(),
        Some("offline")
    );
}

#[test]
fn floating_point_diagnostic_values_remain_numbers() {
    let value = diagnostic_value(DiagnosticValue::F64(f64::NAN));
    assert_eq!(value.kind, "number");
    assert!(value.number_value.unwrap().is_nan());
}

#[test]
fn diagnostic_integers_preserve_their_full_precision() {
    let value = diagnostic_value(DiagnosticValue::U64(u64::MAX));
    assert_eq!(value.kind, "integer");
    assert_eq!(value.string_value.as_deref(), Some("18446744073709551615"));
    assert_eq!(value.number_value, None);
}
