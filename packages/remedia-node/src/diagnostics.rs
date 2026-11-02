use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::UNIX_EPOCH;

use napi::bindgen_prelude::{Function, Status};
use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use napi_derive::napi;
use remedia_observability::{DiagnosticEvent, DiagnosticValue, Severity};

use crate::native::NativeRemedia;
use crate::runtime::{DebugForwarder, lock};
use crate::types::{NativeDiagnosticEvent, NativeDiagnosticField};
use crate::util::napi_error;
use crate::{DIAGNOSTIC_RECEIVE_TIMEOUT, NAPI_DIAGNOSTIC_QUEUE_CAPACITY};

#[napi]
impl NativeRemedia {
    /// Forward this context's bounded diagnostic queue to one JavaScript callback.
    #[napi]
    pub fn set_debug_callback(
        &self,
        callback: Function<'_, NativeDiagnosticEvent, ()>,
    ) -> napi::Result<()> {
        self.inner.ensure_open()?;
        let mut forwarder = lock(&self.inner.debug_forwarder);
        if forwarder.is_some() {
            return Err(napi_error(
                Status::InvalidArg,
                "a debug callback is already registered for this Remedia instance",
            ));
        }

        let callback = callback
            .build_threadsafe_function::<NativeDiagnosticEvent>()
            .weak::<true>()
            .max_queue_size::<NAPI_DIAGNOSTIC_QUEUE_CAPACITY>()
            .build()?;
        let receiver = self.inner.diagnostics.clone();
        let dropped = Arc::clone(&self.inner.diagnostic_drops);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("remedia-node-diagnostics".to_owned())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match receiver.recv_timeout(DIAGNOSTIC_RECEIVE_TIMEOUT) {
                        Ok(event) => {
                            let status = callback.call(
                                to_native_event(event),
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                            if status != Status::Ok {
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(
                            flume::RecvTimeoutError::Timeout
                            | flume::RecvTimeoutError::Disconnected,
                        ) => {}
                    }
                }
            })
            .map_err(|error| {
                napi_error(
                    Status::GenericFailure,
                    format!("could not start diagnostic forwarding worker: {error}"),
                )
            })?;
        *forwarder = Some(DebugForwarder {
            stop,
            worker: Some(worker),
        });
        Ok(())
    }

    /// Number of events discarded by the observability queue or full N-API callback queue.
    #[napi(getter)]
    pub fn dropped_debug_events(&self) -> f64 {
        self.inner
            .diagnostics
            .dropped_events()
            .saturating_add(self.inner.diagnostic_drops.load(Ordering::Relaxed)) as f64
    }
}

pub(crate) fn to_native_event(event: DiagnosticEvent) -> NativeDiagnosticEvent {
    let fields = event
        .fields
        .into_iter()
        .map(|(key, value)| (key, diagnostic_value(value)))
        .collect();
    NativeDiagnosticEvent {
        timestamp: event
            .timestamp
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0,
        severity: severity_name(event.severity).to_owned(),
        target: event.target,
        event: event.event,
        message: event.message,
        playback_id: event.playback_id.map(|id| id.to_string()),
        voice_connection_id: event.voice_connection_id.map(|id| id.to_string()),
        fields,
    }
}

pub(crate) fn diagnostic_value(value: DiagnosticValue) -> NativeDiagnosticField {
    match value {
        DiagnosticValue::String(value) => NativeDiagnosticField {
            kind: "string".to_owned(),
            string_value: Some(value),
            bool_value: None,
            number_value: None,
        },
        DiagnosticValue::Debug(value) => NativeDiagnosticField {
            kind: "debug".to_owned(),
            string_value: Some(value),
            bool_value: None,
            number_value: None,
        },
        DiagnosticValue::Bool(value) => NativeDiagnosticField {
            kind: "boolean".to_owned(),
            string_value: None,
            bool_value: Some(value),
            number_value: None,
        },
        DiagnosticValue::I64(value) => NativeDiagnosticField {
            kind: "integer".to_owned(),
            string_value: Some(value.to_string()),
            bool_value: None,
            number_value: None,
        },
        DiagnosticValue::U64(value) => NativeDiagnosticField {
            kind: "integer".to_owned(),
            string_value: Some(value.to_string()),
            bool_value: None,
            number_value: None,
        },
        DiagnosticValue::F64(value) => NativeDiagnosticField {
            kind: "number".to_owned(),
            string_value: None,
            bool_value: None,
            number_value: Some(value),
        },
    }
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Trace => "trace",
        Severity::Debug => "debug",
        Severity::Info => "info",
        Severity::Warn => "warn",
        Severity::Error => "error",
    }
}
