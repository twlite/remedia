use std::io;
use std::time::Duration;

use opentelemetry::trace::{Span as _, Tracer, TracerProvider as _};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData, SpanExporter};
use opentelemetry_sdk::{metrics::SdkMeterProvider, trace::SdkTracerProvider};
use tracing::Dispatch;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

use super::{ObservabilityError, ObservabilityHandle};
use crate::diagnostics::DiagnosticsLayer;
use crate::metrics::RemediaMetrics;
use crate::{Observability, ObservabilityConfig};

#[derive(Debug)]
struct FailingExporter;

impl SpanExporter for FailingExporter {
    async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
        Err(OTelSdkError::InternalFailure(
            "expected test failure".into(),
        ))
    }

    fn force_flush(&self) -> OTelSdkResult {
        Err(OTelSdkError::InternalFailure(
            "expected flush failure".into(),
        ))
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Err(OTelSdkError::InternalFailure(
            "expected shutdown failure".into(),
        ))
    }
}

fn with_tracer_provider(tracer_provider: SdkTracerProvider) -> (Observability, WorkerGuard) {
    let (writer, writer_guard) = NonBlockingBuilder::default().lossy(true).finish(io::sink());
    let console_error_counter = writer.error_counter();
    let (diagnostics_layer, diagnostics) = DiagnosticsLayer::bounded(8);
    let filter = EnvFilter::new("off");
    let console = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_filter(filter.clone());
    let events = diagnostics_layer.with_filter(filter);
    let layers: Box<dyn Layer<Registry> + Send + Sync> = Box::new(console.and_then(events));
    let dispatch = Dispatch::new(Registry::default().with(layers));
    let meter_provider = SdkMeterProvider::builder().build();
    let handle = ObservabilityHandle {
        dispatch: Some(dispatch.clone()),
        metrics: RemediaMetrics::default(),
    };
    (
        Observability {
            dispatch,
            handle,
            diagnostics,
            tracer_provider: Some(tracer_provider),
            meter_provider: Some(meter_provider),
            writer_guard: None,
            console_error_counter: Some(console_error_counter),
        },
        writer_guard,
    )
}

#[test]
fn disabled_otel_does_not_create_exporters_and_shutdown_is_repeatable() {
    let mut telemetry = Observability::new(ObservabilityConfig::default()).unwrap();
    assert!(!telemetry.handle().metrics().is_enabled());
    telemetry.force_flush().unwrap();
    telemetry.shutdown().unwrap();
    telemetry.shutdown().unwrap();
}

#[test]
fn local_initialization_can_be_repeated_without_global_subscriber_changes() {
    let first = Observability::new(ObservabilityConfig::default()).unwrap();
    let second = Observability::new(ObservabilityConfig::default()).unwrap();
    assert!(first.handle().dispatch().is_some());
    assert!(second.handle().dispatch().is_some());
}

#[test]
fn console_can_be_disabled_while_structured_diagnostics_remain_enabled() {
    let config = ObservabilityConfig::default()
        .with_filter("info")
        .unwrap()
        .with_console_enabled(false);
    let telemetry = Observability::new(config).unwrap();
    let diagnostics = telemetry.diagnostics();
    let dispatch = telemetry.handle().dispatch().unwrap().clone();

    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info!(
            name: "core.test.diagnostics_only",
            target: "remedia_core::test",
            "structured event remains active"
        );
    });

    let event = diagnostics.try_recv().unwrap();
    assert_eq!(event.event, "core.test.diagnostics_only");
    assert_eq!(telemetry.dropped_console_lines(), 0);
}

#[test]
fn explicit_global_install_is_repeatable_without_replacing_the_subscriber() {
    let telemetry = Observability::new(ObservabilityConfig::default()).unwrap();
    telemetry.try_install_global().unwrap();
    assert!(telemetry.try_install_global().is_err());
}

#[test]
fn info_lifecycle_spans_export_even_with_warn_console_filter() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("remedia.test");
    let (writer, _writer_guard) = NonBlockingBuilder::default().lossy(true).finish(io::sink());
    let console = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_filter(EnvFilter::new("warn"));
    let (diagnostics_layer, _diagnostics) = DiagnosticsLayer::bounded(8);
    let diagnostics = diagnostics_layer.with_filter(EnvFilter::new("warn"));
    let otel = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(EnvFilter::new(super::OTEL_FILTER));
    let layers: Box<dyn Layer<Registry> + Send + Sync> =
        Box::new(console.and_then(diagnostics).and_then(otel));
    let dispatch = Dispatch::new(Registry::default().with(layers));

    tracing::dispatcher::with_default(&dispatch, || {
        let span = tracing::info_span!(
            target: "remedia_core::test",
            "core.test.lifecycle"
        );
        let _entered = span.enter();
        tracing::info!(name: "core.test.ready", target: "remedia_core::test", "ready");
    });
    provider.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    assert!(spans.iter().any(|span| span.name == "core.test.lifecycle"));
    provider.shutdown().unwrap();
}

#[test]
fn exporter_flush_and_shutdown_errors_are_returned_to_the_owner() {
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(FailingExporter)
        .build();
    let (mut telemetry, _writer_guard) = with_tracer_provider(tracer_provider);
    let tracer = telemetry
        .tracer_provider
        .as_ref()
        .unwrap()
        .tracer("remedia.test");
    tracer.start("expected-export-failure").end();
    assert!(matches!(
        telemetry.force_flush(),
        Err(ObservabilityError::Export(_))
    ));
    tracer.start("expected-shutdown-export-failure").end();
    assert!(matches!(
        telemetry.shutdown(),
        Err(ObservabilityError::Shutdown(_))
    ));
    telemetry.shutdown().unwrap();
}
