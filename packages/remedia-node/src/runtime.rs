use napi::bindgen_prelude::Status;
use remedia_core::{
    ByteStreamWriter, CancellationToken, InputSource, PlaybackError, PlaybackSession,
};
use remedia_discord::{DiscordOpusSenderRouter, DiscordVoiceConnection};
use remedia_observability::{DiagnosticReceiver, Observability, ObservabilityHandle};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::NativeResult;
use crate::native::NativeRemedia;
use crate::util::{is_active, napi_error};

pub(crate) struct RuntimeInner {
    pub(crate) observability: Mutex<Option<Observability>>,
    pub(crate) observability_handle: ObservabilityHandle,
    pub(crate) diagnostics: DiagnosticReceiver,
    pub(crate) debug_forwarder: Mutex<Option<DebugForwarder>>,
    pub(crate) diagnostic_drops: Arc<AtomicU64>,
    pub(crate) playbacks: Mutex<HashMap<String, PlaybackEntry>>,
    pub(crate) voices: Mutex<HashMap<String, VoiceEntry>>,
    pub(crate) byte_streams: Mutex<HashMap<String, ByteStreamEntry>>,
    pub(crate) next_id: AtomicU64,
    pub(crate) closed: AtomicBool,
    pub(crate) operations: napi::tokio::sync::RwLock<()>,
    pub(crate) close_lock: napi::tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub(crate) struct PlaybackEntry {
    pub(crate) session: Arc<PlaybackSession>,
    pub(crate) voice_id: Option<String>,
    pub(crate) sender_router: Option<DiscordOpusSenderRouter>,
    pub(crate) lifecycle: Arc<Mutex<PlaybackLifecycle>>,
}

#[derive(Default)]
pub(crate) struct PlaybackLifecycle {
    disposed: bool,
    starting: bool,
    cancellation: Option<CancellationToken>,
}

impl PlaybackEntry {
    pub(crate) fn new(
        session: PlaybackSession,
        voice_id: Option<String>,
        sender_router: Option<DiscordOpusSenderRouter>,
    ) -> Self {
        Self {
            session: Arc::new(session),
            voice_id,
            sender_router,
            lifecycle: Arc::new(Mutex::new(PlaybackLifecycle::default())),
        }
    }

    pub(crate) fn begin_start(&self) -> NativeResult<CancellationToken> {
        let mut lifecycle = lock(&self.lifecycle);
        if lifecycle.disposed {
            return Err(napi_error(
                Status::InvalidArg,
                "playback session has been disposed",
            ));
        }
        if lifecycle.starting || is_active(self.session.state()) {
            return Err(napi_error(
                Status::GenericFailure,
                "playback is already active; stop it before starting another source",
            ));
        }
        let cancellation = CancellationToken::new();
        lifecycle.starting = true;
        lifecycle.cancellation = Some(cancellation.clone());
        Ok(cancellation)
    }

    pub(crate) fn finish_start(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        lifecycle.starting = false;
        lifecycle.cancellation = None;
    }

    pub(crate) fn cancel_start(&self) {
        let lifecycle = lock(&self.lifecycle);
        if let Some(cancellation) = lifecycle.cancellation.as_ref() {
            cancellation.cancel();
        }
    }

    pub(crate) fn mark_disposed(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        lifecycle.disposed = true;
        if let Some(cancellation) = lifecycle.cancellation.as_ref() {
            cancellation.cancel();
        }
    }

    pub(crate) fn ensure_available(&self) -> NativeResult<()> {
        if lock(&self.lifecycle).disposed {
            Err(napi_error(
                Status::InvalidArg,
                "playback session has been disposed",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub(crate) struct VoiceEntry {
    pub(crate) connection: DiscordVoiceConnection,
    pub(crate) playback_id: String,
    pub(crate) sender_router: DiscordOpusSenderRouter,
    pub(crate) transport_operations: Arc<napi::tokio::sync::Mutex<()>>,
    pub(crate) transport_generation: Arc<AtomicU64>,
}

#[derive(Clone)]
pub(crate) struct ByteStreamEntry {
    pub(crate) source: InputSource,
    pub(crate) writer: ByteStreamWriter,
}

pub(crate) struct DebugForwarder {
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) worker: Option<JoinHandle<()>>,
}

impl DebugForwarder {
    pub(crate) fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl RuntimeInner {
    pub(crate) fn ensure_open(&self) -> NativeResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(napi_error(
                Status::GenericFailure,
                "Remedia instance is closed",
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    pub(crate) fn playback(&self, id: &str) -> NativeResult<PlaybackEntry> {
        lock(&self.playbacks)
            .get(id)
            .cloned()
            .ok_or_else(|| napi_error(Status::InvalidArg, "unknown playback session"))
    }

    pub(crate) fn voice(&self, id: &str) -> NativeResult<VoiceEntry> {
        lock(&self.voices)
            .get(id)
            .cloned()
            .ok_or_else(|| napi_error(Status::InvalidArg, "unknown Discord voice session"))
    }

    pub(crate) fn byte_stream(&self, id: &str) -> NativeResult<ByteStreamEntry> {
        lock(&self.byte_streams)
            .get(id)
            .cloned()
            .ok_or_else(|| napi_error(Status::InvalidArg, "unknown or closed byte stream"))
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let playbacks = std::mem::take(
            self.playbacks
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let voices = std::mem::take(
            self.voices
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let byte_streams = std::mem::take(
            self.byte_streams
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let observability = self
            .observability
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let forwarder = self
            .debug_forwarder
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        // N-API finalizers may run on Node's thread. Cleanup joins decoders and async transport
        // workers on this best-effort reaper thread instead of blocking the event loop.
        let cleanup = move || {
            for entry in byte_streams.into_values() {
                entry.writer.cancel();
            }
            for entry in playbacks.into_values() {
                entry.cancel_start();
                let _ = entry.session.stop();
            }
            if let Ok(runtime) = napi::tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(async move {
                    for entry in voices.into_values() {
                        let _ = entry.connection.disconnect().await;
                    }
                });
            }
            if let Some(forwarder) = forwarder {
                forwarder.shutdown();
            }
            if let Some(mut observability) = observability {
                let _ = observability.shutdown();
            }
        };

        match thread::Builder::new()
            .name("remedia-node-cleanup".to_owned())
            .spawn(cleanup)
        {
            Ok(_) => {}
            Err(error) => tracing::error!(
                target: "remedia_node",
                error = %error,
                "could not start deferred native cleanup"
            ),
        }
    }
}

impl NativeRemedia {
    pub(crate) async fn with_playback_operation<T, F>(
        &self,
        id: &str,
        operation: F,
    ) -> Result<T, PlaybackError>
    where
        T: Send + 'static,
        F: FnOnce(&PlaybackSession) -> Result<T, PlaybackError> + Send + 'static,
    {
        self.inner
            .ensure_open()
            .map_err(|_| PlaybackError::Cancelled)?;
        let _runtime_operation = self.inner.operations.read().await;
        self.inner
            .ensure_open()
            .map_err(|_| PlaybackError::Cancelled)?;
        let entry = self
            .inner
            .playback(id)
            .map_err(|_| PlaybackError::Cancelled)?;
        entry
            .ensure_available()
            .map_err(|_| PlaybackError::Cancelled)?;
        let session = Arc::clone(&entry.session);
        run_blocking(move || operation(&session))
            .await
            .map_err(|_| PlaybackError::WorkerPanicked)?
    }
}

pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> NativeResult<T> {
    napi::tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| {
            napi_error(
                Status::GenericFailure,
                format!("native worker failed: {error}"),
            )
        })
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
