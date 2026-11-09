use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use napi::bindgen_prelude::Status;
use napi_derive::napi;
use remedia_observability::{Observability, ObservabilityConfig};

use crate::runtime::{RuntimeInner, lock, run_blocking};
use crate::util::{napi_error, observability_error};

#[napi]
pub struct NativeRemedia {
    pub(crate) inner: Arc<RuntimeInner>,
}

#[napi]
impl NativeRemedia {
    /// Create an isolated, diagnostics-only Remedia context from environment configuration.
    #[napi(factory)]
    pub async fn create() -> napi::Result<NativeRemedia> {
        let config = ObservabilityConfig::from_env()
            .map_err(|error| {
                napi_error(
                    Status::InvalidArg,
                    format!("invalid observability configuration: {error}"),
                )
            })?
            .with_console_enabled(false);
        let observability = Observability::new(config)
            .map_err(|error| observability_error("could not initialize observability", error))?;
        let diagnostics = observability.diagnostics();
        let observability_handle = observability.handle();

        Ok(Self {
            inner: Arc::new(RuntimeInner {
                observability: Mutex::new(Some(observability)),
                observability_handle,
                diagnostics,
                debug_forwarder: Mutex::new(None),
                diagnostic_drops: Arc::new(AtomicU64::new(0)),
                playbacks: Mutex::new(HashMap::new()),
                voices: Mutex::new(HashMap::new()),
                probes: Mutex::new(HashMap::new()),
                byte_streams: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                closed: AtomicBool::new(false),
                operations: napi::tokio::sync::RwLock::new(()),
                close_lock: napi::tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Return cached process resources as JSON. All embedded instances share
    /// these process counters. Collection runs off the caller; unsupported
    /// values and the first CPU utilization sample are null.
    /// @returns A JSON process snapshot with byte and millisecond units.
    #[napi]
    pub fn process_resources_json(&self) -> napi::Result<String> {
        serde_json::to_string(&remedia_resources::cached_snapshot())
            .map_err(|error| napi_error(Status::GenericFailure, error.to_string()))
    }

    /// Stop all playback, disconnect all voice transports, flush telemetry, and close the context.
    /// This method is safe to call repeatedly.
    #[napi]
    pub async fn close(&self) -> napi::Result<()> {
        let _close = self.inner.close_lock.lock().await;
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        for token in lock(&self.inner.probes).values() {
            token.cancel();
        }
        let playbacks: Vec<_> = lock(&self.inner.playbacks).values().cloned().collect();
        let voices: Vec<_> = lock(&self.inner.voices).values().cloned().collect();
        let byte_streams = std::mem::take(&mut *lock(&self.inner.byte_streams));
        for stream in byte_streams.into_values() {
            stream.writer.cancel();
        }
        for playback in &playbacks {
            playback.cancel_start();
        }
        let stop_result = run_blocking(move || {
            playbacks
                .into_iter()
                .filter_map(|entry| entry.session.stop().err().map(|error| error.to_string()))
                .collect::<Vec<_>>()
        })
        .await;

        let mut close_errors = match stop_result {
            Ok(errors) => errors,
            Err(error) => vec![error.to_string()],
        };
        for voice in voices {
            if let Err(error) = voice.connection.disconnect().await {
                close_errors.push(error.to_string());
            }
        }

        // Close prevents new calls from acquiring a read guard. Existing operations are cancelled
        // above, then this writer guard waits for their Promise futures to finish before providers
        // and callbacks are released.
        let _exclusive = self.inner.operations.write().await;
        lock(&self.inner.playbacks).clear();
        lock(&self.inner.voices).clear();
        let forwarder = lock(&self.inner.debug_forwarder).take();
        let observability = lock(&self.inner.observability).take();
        let shutdown_result = run_blocking(move || {
            let mut errors = Vec::new();
            if let Some(forwarder) = forwarder {
                forwarder.shutdown();
            }
            if let Some(mut observability) = observability
                && let Err(error) = observability.shutdown()
            {
                errors.push(error.to_string());
            }
            errors
        })
        .await;
        let shutdown_errors = match shutdown_result {
            Ok(errors) => errors,
            Err(error) => vec![error.to_string()],
        };
        close_errors.extend(shutdown_errors);
        if close_errors.is_empty() {
            Ok(())
        } else {
            Err(napi_error(
                Status::GenericFailure,
                format!("Remedia cleanup failed: {}", close_errors.join("; ")),
            ))
        }
    }
}
