use crate::native::NativeRemedia;
use crate::runtime::{lock, run_blocking};
use crate::util::napi_error;
use napi::bindgen_prelude::Status;
use napi_derive::napi;

#[napi]
impl NativeRemedia {
    /// Register a cancellable probe; at most eight operations may be outstanding.
    /// @returns A disposable native operation identifier; rejects if closed or at capacity.
    #[napi]
    pub fn create_probe(&self) -> napi::Result<String> {
        self.inner.ensure_open()?;
        let mut probes = lock(&self.inner.probes);
        if probes.len() >= 8 {
            return Err(napi_error(
                Status::GenericFailure,
                "probe concurrency limit reached",
            ));
        }
        let id = self.inner.id();
        probes.insert(id.clone(), remedia_core::CancellationToken::new());
        Ok(id)
    }
    /// Cancel and release a disposable probe handle. Repeated calls are harmless.
    /// @param id - Operation identifier returned by createProbe.
    #[napi]
    pub fn cancel_probe(&self, id: String) {
        if let Some(token) = lock(&self.inner.probes).remove(&id) {
            token.cancel();
        }
    }
    /// Inspect encoded input asynchronously without creating playback resources.
    /// @param id - Disposable operation identifier returned by createProbe.
    /// @param source - Encoded file, URL, owned bytes or bounded byte-stream handle.
    /// @param timeoutMs - Positive total deadline in milliseconds.
    /// @returns Available metadata; rejects invalid media, timeout, cancellation or capacity exhaustion.
    #[napi]
    pub async fn probe(
        &self,
        id: String,
        source: crate::types::NativeMediaSource,
        timeout_ms: u32,
    ) -> napi::Result<crate::types::NativeStreamInfo> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let cancellation = lock(&self.inner.probes)
            .get(&id)
            .cloned()
            .ok_or_else(|| napi_error(Status::InvalidArg, "probe was cancelled"))?;
        static SLOTS: napi::tokio::sync::Semaphore = napi::tokio::sync::Semaphore::const_new(8);
        let permit = SLOTS
            .try_acquire()
            .map_err(|_| napi_error(Status::GenericFailure, "probe concurrency limit reached"))?;
        let input = if source.kind == "stream" {
            self.inner
                .byte_stream(
                    source
                        .value
                        .as_deref()
                        .ok_or_else(|| napi_error(Status::InvalidArg, "missing stream handle"))?,
                )?
                .source
        } else {
            crate::util::parse_source(source)?
        };
        let handle = self.inner.observability_handle.clone();
        let result = run_blocking(move || {
            let _permit = permit;
            let operation = || {
                remedia_core::probe(
                    input,
                    remedia_core::ProbeOptions {
                        timeout: std::time::Duration::from_millis(u64::from(timeout_ms)),
                        cancellation,
                    },
                )
            };
            match handle.dispatch() {
                Some(dispatch) => tracing::dispatcher::with_default(dispatch, operation),
                None => operation(),
            }
        })
        .await;
        lock(&self.inner.probes).remove(&id);
        result?
            .map(crate::util::native_stream_info)
            .map_err(|error| napi_error(Status::GenericFailure, error.to_string()))
    }
}
