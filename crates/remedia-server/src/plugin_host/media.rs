//! Fetching host-served media with the server's own pinned, redirect-free HTTP client.
//!
//! FFmpeg never talks to a plugin host's media origin. The server redeems the URL,
//! validates the response and feeds a bounded forward-only byte stream to the
//! decoder, so DNS answers and redirects are never re-resolved by a second client.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use remedia_core::{ByteStreamWriter, InputSource};
use tokio::runtime::Handle;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use super::error::{PluginError, PluginErrorKind};
use super::registry::{HostState, send_cancellable};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Cancels an in-flight media transfer unless ownership has passed to a decoder.
///
/// Dropping an armed guard cancels the pump and its byte stream, which releases the
/// upstream connection and the active-stream permit. A decoder that was started
/// from the stream observes closure on its own, so callers disarm after startup.
pub(crate) struct MediaGuard {
    token: CancellationToken,
    writer: ByteStreamWriter,
    armed: bool,
}

impl MediaGuard {
    /// Leave the transfer running; the decoder now owns its lifetime.
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for MediaGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
            self.writer.cancel();
        }
    }
}

enum PumpEnd {
    Finished,
    Cancelled,
    Failed(&'static str),
}

/// Redeem `url` and return a forward-only decoder input fed by a dedicated pump thread.
pub(crate) async fn open_media(
    host: &Arc<HostState>,
    url: url::Url,
    cancel: &CancellationToken,
    permit: OwnedSemaphorePermit,
) -> Result<(InputSource, MediaGuard), PluginError> {
    host.ensure_literal_ip_allowed(&url)?;

    // Credentials are deliberately absent: the media URL is its own bearer capability
    // and may be served by a different origin than the execution endpoint.
    let request = host.client().get(url).header("accept", "*/*");
    let response = send_cancellable(
        request,
        cancel,
        host.registry_shutdown(),
        host.config().limits.request_timeout,
    )
    .await
    .map_err(|error| match error.kind {
        PluginErrorKind::HostUnavailable | PluginErrorKind::Timeout => PluginError::new(
            PluginErrorKind::MediaFailed,
            "media origin could not be reached",
        ),
        _ => error,
    })?;
    let status = response.status().as_u16();

    match status {
        200 => {}
        404 | 410 => {
            return Err(PluginError::new(
                PluginErrorKind::MediaExpired,
                "media grant is expired or was already redeemed",
            ));
        }
        401 | 403 => {
            return Err(PluginError::new(
                PluginErrorKind::MediaInvalid,
                "media origin rejected the media request",
            ));
        }
        300..=399 => {
            return Err(PluginError::new(
                PluginErrorKind::MediaInvalid,
                "media origin attempted a redirect, which is never followed",
            ));
        }
        _ => {
            return Err(PluginError::new(
                PluginErrorKind::MediaFailed,
                "media origin returned an unexpected status",
            ));
        }
    }

    let limits = &host.config().limits;

    if response
        .content_length()
        .is_some_and(|length| length > limits.max_media_bytes)
    {
        return Err(PluginError::new(
            PluginErrorKind::MediaInvalid,
            "media response exceeds the configured size limit",
        ));
    }

    let declared = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());

    if declared.as_deref().is_some_and(|value| {
        value.starts_with("text/")
            || value.starts_with("application/json")
            || value.starts_with("application/xml")
    }) {
        return Err(PluginError::new(
            PluginErrorKind::MediaInvalid,
            "media response is not encoded audio",
        ));
    }

    let (source, writer) = InputSource::byte_stream(limits.media_buffer_bytes).map_err(|_| {
        PluginError::new(
            PluginErrorKind::MediaFailed,
            "media buffer could not be created",
        )
    })?;
    let token = CancellationToken::new();
    let guard = MediaGuard {
        token: token.clone(),
        writer: writer.clone(),
        armed: true,
    };
    let state = Arc::clone(host);
    let handle = Handle::current();
    let observability = host.observability().clone();

    // A dedicated thread owns the blocking, backpressured writes so a paused player
    // cannot exhaust the async runtime's blocking pool.
    std::thread::Builder::new()
        .name("remedia-plugin-media".into())
        .spawn(move || {
            let _permit = permit;
            let end = super::registry::with_dispatch(&observability, || {
                handle.block_on(pump(&state, response, &writer, &token))
            });

            match end {
                PumpEnd::Finished => {
                    let _ = writer.finish();
                }
                PumpEnd::Cancelled => writer.cancel(),
                PumpEnd::Failed(message) => {
                    state.stats().media_failures.fetch_add(1, Ordering::Relaxed);
                    let _ = writer.fail(message);
                }
            }
        })
        .map_err(|_| {
            PluginError::new(
                PluginErrorKind::MediaFailed,
                "media transfer thread could not be started",
            )
        })?;
    host.stats().grants_opened.fetch_add(1, Ordering::Relaxed);

    Ok((source, guard))
}

async fn pump(
    host: &Arc<HostState>,
    mut response: reqwest::Response,
    writer: &ByteStreamWriter,
    token: &CancellationToken,
) -> PumpEnd {
    let limits = &host.config().limits;
    let mut total = 0_u64;

    loop {
        let read = response.chunk();

        tokio::pin!(read);

        let deadline = tokio::time::Instant::now() + limits.media_idle_timeout;
        let chunk = loop {
            tokio::select! {
                biased;
                () = token.cancelled() => return PumpEnd::Cancelled,
                () = host.registry_shutdown().cancelled() => return PumpEnd::Cancelled,
                result = &mut read => break result,
                () = tokio::time::sleep(POLL_INTERVAL) => {
                    if writer.is_closed() {
                        return PumpEnd::Cancelled;
                    }

                    if tokio::time::Instant::now() >= deadline {
                        return PumpEnd::Failed("media transfer stalled");
                    }
                }
            }
        };

        match chunk {
            Ok(Some(bytes)) => {
                total += bytes.len() as u64;

                if total > limits.max_media_bytes {
                    return PumpEnd::Failed("media exceeded the configured size limit");
                }

                host.stats()
                    .media_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);

                // Blocks while the decoder is behind; this is the backpressure point.
                if writer.write_owned(bytes.to_vec()).is_err() {
                    return PumpEnd::Cancelled;
                }
            }
            Ok(None) => return PumpEnd::Finished,
            Err(_) => return PumpEnd::Failed("media transfer was interrupted"),
        }
    }
}
