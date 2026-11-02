//! Opt-in logging, tracing, metrics, and bounded diagnostic-event routing for Remedia.
//!
//! Core and transport crates emit `tracing` events without installing a process-wide subscriber.
//! Applications can use [`Observability::from_env`] to create an owned local dispatcher, then pass
//! its [`ObservabilityHandle`] to playback sessions or voice connections. Installing that
//! dispatcher globally is explicit and fallible.

mod config;
mod diagnostics;
mod metrics;
mod telemetry;

pub use config::{ConfigError, LogFormat, ObservabilityConfig};
pub use diagnostics::{DiagnosticEvent, DiagnosticReceiver, DiagnosticValue, Severity};
pub use metrics::RemediaMetrics;
pub use telemetry::{
    GlobalSubscriberError, Observability, ObservabilityError, ObservabilityHandle,
};

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CORRELATION_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a process-unique numeric ID for Remedia playback and voice spans.
///
/// These IDs are internal correlations. They are unrelated to Discord's secret voice session ID.
pub fn next_correlation_id() -> u64 {
    NEXT_CORRELATION_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::next_correlation_id;

    #[test]
    fn correlation_ids_are_unique_across_threads() {
        let ids = Arc::new(Mutex::new(Vec::new()));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let ids = Arc::clone(&ids);
                thread::spawn(move || {
                    let local: Vec<_> = (0..128).map(|_| next_correlation_id()).collect();
                    ids.lock().unwrap().extend(local);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let ids = ids.lock().unwrap();
        assert_eq!(ids.len(), 1024);
        assert_eq!(ids.iter().copied().collect::<HashSet<_>>().len(), ids.len());
    }
}
