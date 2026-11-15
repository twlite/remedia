use std::io;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use thiserror::Error;
use tracing::Dispatch;
use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

use crate::config::{ConfigError, LogFormat, ObservabilityConfig, OtlpProtocol};
use crate::diagnostics::{DiagnosticReceiver, DiagnosticsLayer};
use crate::metrics::RemediaMetrics;

const DIAGNOSTIC_QUEUE_CAPACITY: usize = 256;
const CONSOLE_QUEUE_CAPACITY: usize = 1024;
const OTEL_FILTER: &str = "remedia_core=info,remedia_discord=info,remedia_observability=info";

/// Per-Remedia instrumentation context shared with core and transport instances.
///
/// The default handle uses the application's current tracing subscriber and no-op metrics. A
/// configured handle adds an owned, local dispatcher and metric instruments without changing any
/// process-global subscriber or OpenTelemetry provider.
#[derive(Clone, Default)]
pub struct ObservabilityHandle {
    dispatch: Option<Dispatch>,
    metrics: RemediaMetrics,
}

impl ObservabilityHandle {
    /// Return the local dispatcher, if this handle was created by [`Observability`].
    pub fn dispatch(&self) -> Option<&Dispatch> {
        self.dispatch.as_ref()
    }

    /// Clone the metrics instruments associated with this handle.
    pub fn metrics(&self) -> RemediaMetrics {
        self.metrics.clone()
    }
}

/// Owns one local tracing dispatcher, diagnostics queue, and optional OTLP providers.
///
/// Constructing this value does not install a global subscriber or provider. Export is enabled
/// only when explicitly requested in configuration. Call [`force_flush`](Self::force_flush) and
/// [`shutdown`](Self::shutdown) during application shutdown.
pub struct Observability {
    dispatch: Dispatch,
    handle: ObservabilityHandle,
    diagnostics: DiagnosticReceiver,
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    writer_guard: Option<WorkerGuard>,
    console_error_counter: Option<ErrorCounter>,
}

impl Observability {
    /// Build an owned observability context from validated configuration.
    pub fn new(config: ObservabilityConfig) -> Result<Self, ObservabilityError> {
        let filter =
            EnvFilter::try_new(config.filter()).map_err(|error| ConfigError::InvalidFilter {
                value: config.filter().to_owned(),
                reason: error.to_string(),
            })?;

        let (diagnostics_layer, diagnostics) = DiagnosticsLayer::bounded(DIAGNOSTIC_QUEUE_CAPACITY);
        let (tracer_provider, meter_provider, metrics, otel_layer) = if config.otel_enabled() {
            let (tracer_provider, meter_provider, metrics) = build_otel(&config)?;
            let tracer = tracer_provider.tracer("remedia");
            // Keep trace collection independent of the console/event filter: the default
            // `warn` console setting must not suppress info-level lifecycle spans.
            let otel_filter = EnvFilter::new(OTEL_FILTER);
            let layer: Box<dyn Layer<Registry> + Send + Sync> = Box::new(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_filter(otel_filter),
            );
            (
                Some(tracer_provider),
                Some(meter_provider),
                metrics,
                Some(layer),
            )
        } else {
            (None, None, RemediaMetrics::default(), None)
        };

        let events: Box<dyn Layer<Registry> + Send + Sync> =
            Box::new(diagnostics_layer.with_filter(filter.clone()));
        let (console, writer_guard, console_error_counter) = if config.console_enabled() {
            let (writer, guard) = NonBlockingBuilder::default()
                .buffered_lines_limit(CONSOLE_QUEUE_CAPACITY)
                .lossy(true)
                .thread_name("remedia-log-writer")
                .finish(io::stderr());
            let error_counter = writer.error_counter();
            let layer: Box<dyn Layer<Registry> + Send + Sync> = match config.log_format() {
                LogFormat::Text => Box::new(
                    tracing_subscriber::fmt::layer()
                        .with_writer(writer)
                        .with_target(true)
                        .with_ansi(false)
                        .with_filter(filter),
                ),
                LogFormat::Json => Box::new(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_writer(writer)
                        .with_target(true)
                        .with_ansi(false)
                        .with_filter(filter),
                ),
            };
            (Some(layer), Some(guard), Some(error_counter))
        } else {
            (None, None, None)
        };

        let mut layers: Box<dyn Layer<Registry> + Send + Sync> = match console {
            Some(console) => Box::new(console.and_then(events)),
            None => events,
        };
        if let Some(otel) = otel_layer {
            layers = Box::new(layers.and_then(otel));
        }
        let dispatch = Dispatch::new(Registry::default().with(layers));
        let handle = ObservabilityHandle {
            dispatch: Some(dispatch.clone()),
            metrics,
        };

        Ok(Self {
            dispatch,
            handle,
            diagnostics,
            tracer_provider,
            meter_provider,
            writer_guard,
            console_error_counter,
        })
    }

    /// Read supported Remedia and OpenTelemetry environment variables and build the context.
    pub fn from_env() -> Result<Self, ObservabilityError> {
        Self::new(ObservabilityConfig::from_env()?)
    }

    /// Clone the context to attach to a playback session or Discord voice connection.
    pub fn handle(&self) -> ObservabilityHandle {
        self.handle.clone()
    }

    /// Clone the bounded event receiver. Receivers compete for events if cloned.
    pub fn diagnostics(&self) -> DiagnosticReceiver {
        self.diagnostics.clone()
    }

    /// Install this dispatcher as the process-wide default if none has been installed.
    ///
    /// This is explicit and returns an error if the application already owns the global
    /// subscriber. It never replaces an existing subscriber and does not panic.
    pub fn try_install_global(&self) -> Result<(), GlobalSubscriberError> {
        tracing::dispatcher::set_global_default(self.dispatch.clone())
            .map_err(|_| GlobalSubscriberError)
    }

    /// Return the number of console lines dropped because the bounded writer was full.
    pub fn dropped_console_lines(&self) -> u64 {
        self.console_error_counter
            .as_ref()
            .map_or(0, |counter| counter.dropped_lines() as u64)
    }

    /// Flush pending traces and metrics. No-op when OTLP is disabled.
    pub fn force_flush(&self) -> Result<(), ObservabilityError> {
        if let Some(provider) = &self.tracer_provider {
            provider
                .force_flush()
                .map_err(|error| ObservabilityError::Export(error.to_string()))?;
        }
        if let Some(provider) = &self.meter_provider {
            provider
                .force_flush()
                .map_err(|error| ObservabilityError::Export(error.to_string()))?;
        }
        Ok(())
    }

    /// Flush and stop owned OTLP providers and release the console writer worker.
    ///
    /// Call this from application shutdown code, not from a real-time audio callback. This method
    /// is idempotent for a given `Observability` value.
    pub fn shutdown(&mut self) -> Result<(), ObservabilityError> {
        let mut errors = Vec::new();
        if let Some(provider) = self.tracer_provider.take() {
            if let Err(error) = provider.force_flush() {
                errors.push(error.to_string());
            }
            if let Err(error) = provider.shutdown() {
                errors.push(error.to_string());
            }
        }
        if let Some(provider) = self.meter_provider.take() {
            if let Err(error) = provider.force_flush() {
                errors.push(error.to_string());
            }
            if let Err(error) = provider.shutdown() {
                errors.push(error.to_string());
            }
        }
        self.writer_guard.take();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ObservabilityError::Shutdown(errors.join("; ")))
        }
    }
}

fn build_otel(
    config: &ObservabilityConfig,
) -> Result<(SdkTracerProvider, SdkMeterProvider, RemediaMetrics), ObservabilityError> {
    let needs_tokio = matches!(config.traces_protocol, OtlpProtocol::Grpc)
        || matches!(config.metrics_protocol, OtlpProtocol::Grpc);
    if needs_tokio && tokio::runtime::Handle::try_current().is_err() {
        return Err(ObservabilityError::RuntimeRequired);
    }

    let span_exporter = match config.traces_protocol {
        OtlpProtocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build(),
        OtlpProtocol::HttpProtobuf => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build(),
        OtlpProtocol::HttpJson => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpJson)
            .build(),
    }
    .map_err(|error| ObservabilityError::Exporter(error.to_string()))?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .build();

    let metric_exporter = match config.metrics_protocol {
        OtlpProtocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .build(),
        OtlpProtocol::HttpProtobuf => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build(),
        OtlpProtocol::HttpJson => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpJson)
            .build(),
    }
    .map_err(|error| ObservabilityError::Exporter(error.to_string()))?;
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .build();
    let metrics = RemediaMetrics::from_meter(meter_provider.meter("remedia"));

    Ok((tracer_provider, meter_provider, metrics))
}

/// The process already has a global tracing subscriber.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("a global tracing subscriber is already installed; use the local ObservabilityHandle")]
pub struct GlobalSubscriberError;

/// An initialization, export, or shutdown failure.
#[derive(Debug, Error)]
pub enum ObservabilityError {
    /// Remedia or OpenTelemetry configuration was invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The OTLP exporter could not be initialized.
    #[error("could not initialize OTLP exporter: {0}")]
    Exporter(String),
    /// An explicit flush failed.
    #[error("telemetry export failed: {0}")]
    Export(String),
    /// One or more providers failed to flush or shut down.
    #[error("telemetry shutdown failed: {0}")]
    Shutdown(String),
    /// gRPC OTLP exporters require an active Tokio runtime during initialization.
    #[error("the gRPC OTLP protocol requires Observability::new to run inside a Tokio runtime")]
    RuntimeRequired,
}

#[cfg(test)]
mod tests;
