//! Authenticated, bounded and policy-checked HTTP access to plugin hosts.
//!
//! Every connection resolves DNS once and permits private addresses only for explicitly
//! configured origins, never follows redirects and never uses proxies. Both the server and
//! the management CLI discover hosts through this module so they enforce identical rules.
use std::net::SocketAddr;
use std::sync::{Arc, Once};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio_util::sync::CancellationToken;

use crate::error::{PluginError, PluginErrorKind};
use crate::protocol::{MANIFEST_PATH, Manifest, error_code, error_from_status, parse_manifest};
use crate::{PluginHostConfig, is_public_ip};

/// Resolves DNS answers once and permits private addresses only for configured hosts,
/// so the validated address set is exactly what the connection uses.
pub(crate) struct PolicyResolver {
    trusted_hosts: Vec<String>,
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.trusted_hosts.iter().any(|host| host == name.as_str());

        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .collect::<Vec<SocketAddr>>();

            if addresses.is_empty() {
                return Err("host did not resolve".into());
            }

            if !allow_private && addresses.iter().any(|address| !is_public_ip(address.ip())) {
                return Err("host resolves to a non-public address".into());
            }

            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub fn install_crypto_provider() {
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        // Another component may already have installed a process default; keep it.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Await a request under cancellation, shutdown and a deadline. Dropping the future
/// closes the connection, which is how a pending operation is actually cancelled.
pub async fn send_cancellable(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
    timeout: Duration,
) -> Result<reqwest::Response, PluginError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(PluginError::new(PluginErrorKind::Cancelled, "operation was cancelled")),
        () = shutdown.cancelled() => Err(PluginError::new(PluginErrorKind::ShuttingDown, "server is shutting down")),
        result = tokio::time::timeout(timeout, request.send()) => match result {
            Err(_) => Err(PluginError::new(PluginErrorKind::Timeout, "plugin host request timed out")),
            Ok(Err(error)) => Err(transport_error(&error)),
            Ok(Ok(response)) => Ok(response),
        },
    }
}

pub fn transport_error(error: &reqwest::Error) -> PluginError {
    if error.is_timeout() {
        PluginError::new(PluginErrorKind::Timeout, "plugin host request timed out")
    } else if error.is_redirect() {
        PluginError::new(
            PluginErrorKind::Incompatible,
            "plugin host attempted a redirect, which is never followed",
        )
    } else {
        use std::error::Error;

        // Inspect causes only to classify them; never expose their text or URL.
        let mut source: Option<&(dyn Error + 'static)> = Some(error);
        let mut refused = false;
        let mut dns = false;
        let mut tls = false;
        let mut mismatch = false;
        let mut blocked = false;

        while let Some(cause) = source {
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                refused |= io.kind() == std::io::ErrorKind::ConnectionRefused;
            }

            let text = cause.to_string().to_ascii_lowercase();
            dns |= text.contains("dns")
                || text.contains("resolve")
                || text.contains("name or service");
            tls |= text.contains("tls") || text.contains("certificate");
            mismatch |= text.contains("invalidcontenttype")
                || text.contains("invalid content type")
                || text.contains("wrong version")
                || text.contains("invalid http version")
                || text.contains("message header");
            blocked |= text.contains("non-public address");
            source = cause.source();
        }

        let detail = if blocked {
            "destination blocked by network policy; check the configured origin"
        } else if refused {
            "plugin host refused the connection; check that it is running and the port is correct"
        } else if mismatch {
            "plugin host protocol mismatch; check whether the URL should use http or https"
        } else if tls {
            "plugin host TLS handshake failed; check the certificate, trust chain and URL scheme"
        } else if dns {
            "plugin host DNS lookup failed; check the hostname and resolver"
        } else {
            "plugin host connection failed; check the URL scheme, port and network reachability"
        };

        PluginError::new(PluginErrorKind::HostUnavailable, detail)
    }
}

/// Read at most `limit` bytes, failing rather than truncating.
pub async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
    timeout: Duration,
) -> Result<Vec<u8>, PluginError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(PluginError::new(
            PluginErrorKind::ResponseTooLarge,
            "plugin host response exceeds the configured limit",
        ));
    }

    let read = async move {
        let mut body = Vec::new();

        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > limit {
                        return Err(PluginError::new(
                            PluginErrorKind::ResponseTooLarge,
                            "plugin host response exceeds the configured limit",
                        ));
                    }

                    body.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(body),
                Err(error) => return Err(transport_error(&error)),
            }
        }
    };

    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(PluginError::new(PluginErrorKind::Cancelled, "operation was cancelled")),
        () = shutdown.cancelled() => Err(PluginError::new(PluginErrorKind::ShuttingDown, "server is shutting down")),
        result = tokio::time::timeout(timeout, read) => match result {
            Err(_) => Err(PluginError::new(PluginErrorKind::Timeout, "plugin host request timed out")),
            Ok(result) => result,
        },
    }
}

/// Build the HTTP client for one deployment: HTTPS-only unless opted out, no redirects,
/// no proxies, pinned DNS policy and a connect timeout.
pub fn build_client(config: &PluginHostConfig) -> Result<reqwest::Client, PluginError> {
    install_crypto_provider();

    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(PolicyResolver {
            trusted_hosts: config
                .media_origins
                .iter()
                .filter_map(|origin| url::Url::parse(origin).ok()?.host_str().map(str::to_owned))
                .collect(),
        }))
        .connect_timeout(config.limits.request_timeout)
        .https_only(!config.allow_http)
        .user_agent(concat!("remedia-server/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| {
            PluginError::new(
                PluginErrorKind::HostFailed,
                "plugin host HTTP client could not be built",
            )
        })
}

/// URL of one of the fixed v1 routes beneath the configured origin.
pub fn endpoint_url(config: &PluginHostConfig, path: &str) -> url::Url {
    // The configured endpoint is an origin; the path is one of the fixed v1 routes.
    let mut url = config.url.clone();

    url.set_path(path);

    url
}

/// Attach the deployment's bearer token. The token is only ever sent to its own origin.
pub fn authorize(
    config: &PluginHostConfig,
    request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    match &config.token {
        Some(token) => request.bearer_auth(token.expose()),
        None => request,
    }
}

/// IP literals bypass the resolver, so they are checked against the same policy here.
pub fn ensure_literal_ip_allowed(
    config: &PluginHostConfig,
    url: &url::Url,
) -> Result<(), PluginError> {
    let literal = match url.host() {
        Some(url::Host::Ipv4(address)) => Some(std::net::IpAddr::V4(address)),
        Some(url::Host::Ipv6(address)) => Some(std::net::IpAddr::V6(address)),
        _ => None,
    };

    if literal.is_some_and(|address| !config.owns_media_origin(url) && !is_public_ip(address)) {
        return Err(PluginError::new(
            PluginErrorKind::MediaOriginNotAllowed,
            "address is not permitted for this plugin host",
        ));
    }

    Ok(())
}

/// Fetch and validate `/.well-known/remedia`. Bounded in time and size; failures are
/// categorized without exposing URLs or credentials.
pub async fn fetch_manifest(
    client: &reqwest::Client,
    config: &PluginHostConfig,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
) -> Result<Manifest, PluginError> {
    fetch_manifest_for(client, config, Some(&config.id), cancel, shutdown).await
}

/// Like [`fetch_manifest`], but with an explicit identity expectation. `None` skips the
/// `hostId` check, for registering a host whose ID is not decided yet.
pub async fn fetch_manifest_for(
    client: &reqwest::Client,
    config: &PluginHostConfig,
    expected_id: Option<&str>,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
) -> Result<Manifest, PluginError> {
    ensure_literal_ip_allowed(config, &config.url).map_err(|_| {
        PluginError::new(
            PluginErrorKind::HostUnavailable,
            "address is not permitted for this plugin host",
        )
    })?;

    let request = authorize(config, client.get(endpoint_url(config, MANIFEST_PATH)));
    let response =
        send_cancellable(request, cancel, shutdown, config.limits.request_timeout).await?;
    let status = response.status();

    if !status.is_success() {
        let body = read_limited(
            response,
            4_096,
            cancel,
            shutdown,
            config.limits.request_timeout,
        )
        .await
        .unwrap_or_default();

        return Err(error_from_status(
            status.as_u16(),
            error_code(&body).as_deref(),
        ));
    }

    let body = read_limited(
        response,
        config.limits.max_response_bytes,
        cancel,
        shutdown,
        config.limits.request_timeout,
    )
    .await?;

    parse_manifest(&body, expected_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn resolver_blocks_unconfigured_private_hosts_and_pins_configured_hosts() {
        let blocked = PolicyResolver {
            trusted_hosts: vec![],
        };
        let trusted = PolicyResolver {
            trusted_hosts: vec!["localhost".into()],
        };

        assert!(blocked.resolve("localhost".parse().unwrap()).await.is_err());
        let addresses = trusted
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap()
            .collect::<Vec<_>>();

        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.ip().is_loopback()));
    }

    #[tokio::test]
    async fn connection_diagnostics_are_actionable_and_redact_urls() {
        install_crypto_provider();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = client
            .get(format!("http://{address}/private?token=secret"))
            .send()
            .await
            .unwrap_err();
        let diagnostic = transport_error(&error);

        assert!(diagnostic.detail.contains("refused"));
        assert!(!diagnostic.detail.contains("secret"));
        assert!(!diagnostic.detail.contains(&address.to_string()));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];

            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let error = client
            .get(format!("https://{address}/private?token=secret"))
            .send()
            .await
            .unwrap_err();
        let diagnostic = transport_error(&error);

        assert!(
            diagnostic.detail.contains("mismatch"),
            "{}",
            diagnostic.detail
        );
        assert!(!diagnostic.detail.contains("secret"));
        server.await.unwrap();
    }
}
