# remedia-observability

Shared, opt-in structured logging, tracing, metrics, and bounded diagnostic event routing for the
Remedia Rust workspace.

Core and transport crates emit `tracing` events without installing a global subscriber. Native
applications can construct an `Observability` context and pass its handle to playback sessions and
Discord voice connections. The context creates a local
dispatcher, bounded text or JSON console output, and optional OTLP trace and metric providers.

```rust,no_run
use remedia_core::PlaybackSession;
use remedia_observability::Observability;

let mut observability = Observability::from_env()?;
let playback = PlaybackSession::with_observability(observability.handle());
// Start playback with the public PlaybackSession API.
let _ = playback;
observability.force_flush()?;
observability.shutdown()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Set `REMEDIA_DEBUG=1` for Remedia debug events, `REMEDIA_LOG` for tracing directives,
`REMEDIA_LOG_FORMAT=json` for JSON console lines, and `REMEDIA_OTEL_ENABLED=true` to opt into
OpenTelemetry export. Standard `OTEL_*` variables configure the service resource, sampling, and
OTLP endpoints. Use `ObservabilityConfig::with_console_enabled(false)` when an embedding host routes
events elsewhere. Diagnostic events inherit playback and voice correlation IDs from active Remedia
spans where applicable. The Node binding forwards structured diagnostics through a bounded
ThreadsafeFunction. OTLP log export is not implemented.

See the [observability guide](../../apps/docs/content/docs/observability/configuration.mdx) and
[architecture overview](../../docs/architecture.md) for initialization, filtering, event routing,
metrics, security, and lifecycle details.
