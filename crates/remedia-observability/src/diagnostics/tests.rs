use std::time::SystemTime;

use tracing::Dispatch;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

use super::DiagnosticsLayer;

#[test]
fn captures_structured_fields_and_redacts_sensitive_values() {
    let (layer, receiver) = DiagnosticsLayer::bounded(256);
    let dispatch =
        Dispatch::new(Registry::default().with(layer.with_filter(EnvFilter::new("trace"))));

    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info!(
            name: "playback.started",
            target: "remedia_core::playback",
            playback_id = 41_u64,
            codec = "pcm_s16le",
            voice_session_id = "do-not-forward",
            secret_key = "never-export-this",
            "Playback is ready from https://user:password@example.test/file?token=private"
        );
    });

    let event = receiver.try_recv().unwrap();
    assert!(
        event
            .timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .is_ok()
    );
    assert_eq!(event.severity, crate::Severity::Info);
    assert_eq!(event.target, "remedia_core::playback");
    assert_eq!(event.event, "playback.started");
    assert_eq!(
        event.message.as_deref(),
        Some("Playback is ready from https://[REDACTED]")
    );
    assert_eq!(event.playback_id, Some(41));
    assert_eq!(
        event.fields["codec"],
        crate::DiagnosticValue::String("pcm_s16le".into())
    );
    assert_eq!(
        event.fields["voice_session_id"],
        crate::DiagnosticValue::String("[REDACTED]".into())
    );
    assert_eq!(
        event.fields["secret_key"],
        crate::DiagnosticValue::String("[REDACTED]".into())
    );
}

#[test]
fn redacts_camel_case_sensitive_field_names() {
    let (layer, receiver) = DiagnosticsLayer::bounded(8);
    let dispatch =
        Dispatch::new(Registry::default().with(layer.with_filter(EnvFilter::new("trace"))));
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info!(
            name: "test.sensitive_fields",
            target: "remedia_core::test",
            authToken = "secret-token",
            sessionId = "secret-session",
            rawPacket = "secret-audio",
            "safe diagnostic"
        );
    });

    let event = receiver.try_recv().unwrap();
    for field in ["authToken", "sessionId", "rawPacket"] {
        assert_eq!(
            event.fields[field],
            crate::DiagnosticValue::String("[REDACTED]".into())
        );
    }
}

#[test]
fn events_inherit_playback_and_voice_correlations_from_spans() {
    let (layer, receiver) = DiagnosticsLayer::bounded(8);
    let dispatch =
        Dispatch::new(Registry::default().with(layer.with_filter(EnvFilter::new("info"))));

    tracing::dispatcher::with_default(&dispatch, || {
        let playback_span = tracing::info_span!(
            target: "remedia_core::playback",
            "core.playback.session",
            playback_id = 45_u64
        );
        playback_span.in_scope(|| {
            tracing::info!(
                name: "core.media.opened",
                target: "remedia_core::decoder",
                "opened"
            );
        });
        let voice_span = tracing::info_span!(
            target: "remedia_discord::connection",
            "discord.connection.worker",
            voice_connection_id = 46_u64
        );
        voice_span.in_scope(|| {
            tracing::info!(
                name: "discord.dave.ready",
                target: "remedia_discord::connection",
                "ready"
            );
        });
    });

    let playback = receiver.try_recv().unwrap();
    assert_eq!(playback.event, "core.media.opened");
    assert_eq!(playback.playback_id, Some(45));
    let voice = receiver.try_recv().unwrap();
    assert_eq!(voice.event, "discord.dave.ready");
    assert_eq!(voice.voice_connection_id, Some(46));
}

#[test]
fn component_filtering_and_bounded_delivery_are_non_blocking() {
    let (layer, receiver) = DiagnosticsLayer::bounded(256);
    let dispatch = Dispatch::new(
        Registry::default().with(layer.with_filter(EnvFilter::new("remedia_core=debug"))),
    );
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::event!(name: "core.visible", target: "remedia_core::decoder", tracing::Level::DEBUG, playback_id = 8_u64, "visible");
        tracing::event!(name: "discord.filtered", target: "remedia_discord::connection", tracing::Level::DEBUG, voice_connection_id = 9_u64, "filtered");
    });
    let event = receiver.try_recv().unwrap();
    assert_eq!(event.event, "core.visible");
    assert!(receiver.try_recv().is_err());

    let (layer, receiver) = DiagnosticsLayer::bounded(256);
    let dispatch =
        Dispatch::new(Registry::default().with(layer.with_filter(EnvFilter::new("trace"))));
    tracing::dispatcher::with_default(&dispatch, || {
        for index in 0..300_u64 {
            tracing::event!(name: "test.bounded", target: "remedia_core::test", tracing::Level::INFO, index, "queued");
        }
    });
    assert_eq!(receiver.dropped_events(), 44);
}
