use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use flume::{Receiver, Sender, TryRecvError};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Severity level for a structured Remedia diagnostic event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// A typed value captured from a tracing event.
#[derive(Clone, Debug, PartialEq)]
pub enum DiagnosticValue {
    String(String),
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Debug(String),
}

/// A bounded structured event that can be consumed by a native host or a future N-API bridge.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticEvent {
    /// Wall-clock time at which the local subscriber observed the event.
    pub timestamp: SystemTime,
    /// Event severity.
    pub severity: Severity,
    /// `tracing` target, normally the emitting Rust module path.
    pub target: String,
    /// Stable event identifier from the tracing callsite name.
    pub event: String,
    /// Human-readable event text, if the callsite supplied one.
    pub message: Option<String>,
    /// Structured callsite fields. Sensitive fields are replaced with `[REDACTED]`.
    pub fields: BTreeMap<String, DiagnosticValue>,
    /// Optional internal playback correlation ID.
    pub playback_id: Option<u64>,
    /// Optional internal voice connection correlation ID.
    pub voice_connection_id: Option<u64>,
}

/// Receiver for the bounded, non-blocking diagnostic event queue.
#[derive(Clone, Debug)]
pub struct DiagnosticReceiver {
    receiver: Receiver<DiagnosticEvent>,
    dropped: Arc<AtomicU64>,
}

impl DiagnosticReceiver {
    /// Wait for the next event. Do not call this from a decoder or audio worker.
    pub fn recv(&self) -> Result<DiagnosticEvent, flume::RecvError> {
        self.receiver.recv()
    }

    /// Wait up to `timeout` for an event without blocking indefinitely.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<DiagnosticEvent, flume::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    /// Read a queued event without waiting.
    pub fn try_recv(&self) -> Result<DiagnosticEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Return the number of events dropped because the bounded queue was full.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticsLayer {
    sender: Sender<DiagnosticEvent>,
    dropped: Arc<AtomicU64>,
}

impl DiagnosticsLayer {
    pub(crate) fn bounded(capacity: usize) -> (Self, DiagnosticReceiver) {
        let (sender, receiver) = flume::bounded(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Self {
                sender,
                dropped: Arc::clone(&dropped),
            },
            DiagnosticReceiver { receiver, dropped },
        )
    }
}

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut visitor = CorrelationVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(visitor.fields);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        if let Some(span) = context.span(id) {
            let mut extensions = span.extensions_mut();
            let mut visitor = CorrelationVisitor {
                fields: extensions
                    .get_mut::<CorrelationFields>()
                    .copied()
                    .unwrap_or_default(),
            };
            values.record(&mut visitor);
            if let Some(fields) = extensions.get_mut::<CorrelationFields>() {
                *fields = visitor.fields;
            } else {
                extensions.insert(visitor.fields);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let playback_id = visitor.fields.get("playback_id").and_then(as_u64);
        let voice_connection_id = visitor.fields.get("voice_connection_id").and_then(as_u64);
        let inherited_correlation = context_correlation(context);
        let diagnostic = DiagnosticEvent {
            timestamp: SystemTime::now(),
            severity: severity(*metadata.level()),
            target: metadata.target().to_owned(),
            event: metadata.name().to_owned(),
            message: visitor.message,
            fields: visitor.fields,
            playback_id: playback_id.or(inherited_correlation.playback_id),
            voice_connection_id: voice_connection_id.or(inherited_correlation.voice_connection_id),
        };

        if self.sender.try_send(diagnostic).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct CorrelationFields {
    playback_id: Option<u64>,
    voice_connection_id: Option<u64>,
}

#[derive(Default)]
struct CorrelationVisitor {
    fields: CorrelationFields,
}

impl CorrelationVisitor {
    fn record(&mut self, field: &Field, value: DiagnosticValue) {
        let value = as_u64(&value);
        match field.name() {
            "playback_id" => self.fields.playback_id = value,
            "voice_connection_id" => self.fields.voice_connection_id = value,
            _ => {}
        }
    }
}

impl Visit for CorrelationVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, DiagnosticValue::Debug(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, DiagnosticValue::String(value.to_owned()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, DiagnosticValue::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, DiagnosticValue::U64(value));
    }
}

fn context_correlation<S>(context: Context<'_, S>) -> CorrelationFields
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let mut correlation = CorrelationFields::default();
    if let Some(current) = context.lookup_current() {
        for span in current.scope().from_root() {
            if let Some(fields) = span.extensions().get::<CorrelationFields>() {
                correlation.playback_id = correlation.playback_id.or(fields.playback_id);
                correlation.voice_connection_id = correlation
                    .voice_connection_id
                    .or(fields.voice_connection_id);
            }
        }
    }
    correlation
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: BTreeMap<String, DiagnosticValue>,
}

impl EventVisitor {
    fn insert(&mut self, field: &Field, value: DiagnosticValue) {
        let name = field.name();
        if name == "message" {
            let message = match value {
                DiagnosticValue::String(value) | DiagnosticValue::Debug(value) => value,
                other => format_value(other),
            };
            self.message = Some(redact_text(&message));
        } else if is_sensitive_field(name) {
            self.fields.insert(
                name.to_owned(),
                DiagnosticValue::String("[REDACTED]".to_owned()),
            );
        } else {
            self.fields.insert(name.to_owned(), redact_value(value));
        }
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, DiagnosticValue::Debug(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, DiagnosticValue::String(value.to_owned()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, DiagnosticValue::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, DiagnosticValue::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, DiagnosticValue::U64(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, DiagnosticValue::F64(value));
    }
}

fn severity(level: tracing::Level) -> Severity {
    match level {
        tracing::Level::TRACE => Severity::Trace,
        tracing::Level::DEBUG => Severity::Debug,
        tracing::Level::INFO => Severity::Info,
        tracing::Level::WARN => Severity::Warn,
        tracing::Level::ERROR => Severity::Error,
    }
}

fn as_u64(value: &DiagnosticValue) -> Option<u64> {
    match value {
        DiagnosticValue::U64(value) => Some(*value),
        DiagnosticValue::I64(value) => u64::try_from(*value).ok(),
        DiagnosticValue::String(value) | DiagnosticValue::Debug(value) => value.parse().ok(),
        DiagnosticValue::Bool(_) | DiagnosticValue::F64(_) => None,
    }
}

fn format_value(value: DiagnosticValue) -> String {
    match value {
        DiagnosticValue::String(value) | DiagnosticValue::Debug(value) => value,
        DiagnosticValue::Bool(value) => value.to_string(),
        DiagnosticValue::I64(value) => value.to_string(),
        DiagnosticValue::U64(value) => value.to_string(),
        DiagnosticValue::F64(value) => value.to_string(),
    }
}

fn is_sensitive_field(name: &str) -> bool {
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut previous_was_lowercase = false;
    for character in name.chars() {
        if !character.is_ascii_alphanumeric() {
            if !piece.is_empty() {
                pieces.push(std::mem::take(&mut piece));
            }
            previous_was_lowercase = false;
        } else {
            if character.is_ascii_uppercase() && previous_was_lowercase && !piece.is_empty() {
                pieces.push(std::mem::take(&mut piece));
            }
            piece.push(character.to_ascii_lowercase());
            previous_was_lowercase = character.is_ascii_lowercase();
        }
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }

    let joined = pieces.join("");
    pieces.iter().any(|piece| {
        matches!(
            piece.as_str(),
            "token"
                | "secret"
                | "credential"
                | "password"
                | "authorization"
                | "apikey"
                | "session"
                | "key"
                | "url"
                | "uri"
                | "endpoint"
                | "source"
                | "payload"
                | "samples"
                | "pcm"
                | "opus"
                | "audio"
        )
    }) || matches!(joined.as_str(), "sessionid" | "rawpacket" | "packetbytes")
}

fn redact_value(value: DiagnosticValue) -> DiagnosticValue {
    match value {
        DiagnosticValue::String(value) => DiagnosticValue::String(redact_text(&value)),
        DiagnosticValue::Debug(value) => DiagnosticValue::Debug(redact_text(&value)),
        other => other,
    }
}

fn redact_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some((index, prefix)) = ["https://", "http://", "wss://", "ws://"]
        .iter()
        .filter_map(|prefix| remaining.find(prefix).map(|index| (index, *prefix)))
        .min_by_key(|(index, _)| *index)
    {
        let url_start = index;
        let url_end = remaining[url_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | ')' | ']' | '}')
            })
            .map_or(remaining.len(), |offset| url_start + offset);
        output.push_str(&remaining[..url_start]);
        output.push_str(prefix);
        output.push_str("[REDACTED]");
        remaining = &remaining[url_end..];
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
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
}
