//! Host registry, discovery, authenticated execution and source resolution.
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use remedia_core::InputSource;
use remedia_observability::ObservabilityHandle;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;

use super::error::{PluginError, PluginErrorKind};
use super::media::{MediaGuard, open_media};
use super::protocol::{
    EXECUTE_PATH, ExecuteOutcome, MANIFEST_PATH, Manifest, error_code, error_from_status,
    parse_descriptor, parse_execute_response, validate_manifest,
};
use crate::config::{MediaAccessError, PluginHostConfig, ServerConfig, is_public_ip};

/// Minimum interval between forced rediscovery attempts triggered by a missing operation.
const REDISCOVERY_FLOOR: Duration = Duration::from_secs(5);
const REDISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(150);
const CLOSE_DRAIN: Duration = Duration::from_secs(5);
const UNHEALTHY_AFTER: u64 = 3;

pub(crate) fn with_dispatch<T>(
    observability: &ObservabilityHandle,
    callback: impl FnOnce() -> T,
) -> T {
    match observability.dispatch() {
        Some(dispatch) => tracing::dispatcher::with_default(dispatch, callback),
        None => callback(),
    }
}

/// Cumulative resource and outcome counters for one host.
#[derive(Default)]
pub(crate) struct HostStats {
    pub(crate) operations: AtomicU64,
    pub(crate) operation_failures: AtomicU64,
    pub(crate) retries: AtomicU64,
    pub(crate) declined: AtomicU64,
    pub(crate) unauthorized: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) cancelled: AtomicU64,
    pub(crate) grants_opened: AtomicU64,
    pub(crate) grants_expired: AtomicU64,
    pub(crate) media_bytes: AtomicU64,
    pub(crate) media_failures: AtomicU64,
    pub(crate) discoveries: AtomicU64,
    pub(crate) discovery_failures: AtomicU64,
}

struct CachedManifest {
    manifest: Arc<Manifest>,
    fetched_at: Instant,
}

#[derive(Default)]
struct Health {
    consecutive_failures: u64,
    last_error: Option<&'static str>,
    last_success_unix_ms: Option<u64>,
    last_failure_unix_ms: Option<u64>,
    discovered: bool,
}

/// One configured deployment and its connection policy.
pub(crate) struct HostState {
    config: Arc<PluginHostConfig>,
    client: reqwest::Client,
    pending: Arc<Semaphore>,
    streams: Arc<Semaphore>,
    manifest: tokio::sync::Mutex<Option<CachedManifest>>,
    health: Mutex<Health>,
    stats: HostStats,
    shutdown: CancellationToken,
    observability: ObservabilityHandle,
}

/// Cheap reference to an operation to execute.
pub(crate) struct OperationRef<'a> {
    pub(crate) plugin_id: &'a str,
    pub(crate) operation: &'a str,
    pub(crate) input: &'a Value,
}

/// A decoder input produced by an operation, plus the guard that owns its transfer.
pub(crate) struct ResolvedMedia {
    pub(crate) source: InputSource,
    pub(crate) guard: Option<MediaGuard>,
}

/// Resolves DNS answers once and rejects non-public addresses unless a deployment opts in,
/// so the validated address set is exactly what the connection uses.
struct PolicyResolver {
    allow_private: bool,
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;

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

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        // Another component may already have installed a process default; keep it.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn operation_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Await a request under cancellation, shutdown and a deadline. Dropping the future
/// closes the connection, which is how a pending operation is actually cancelled.
pub(crate) async fn send_cancellable(
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

fn transport_error(error: &reqwest::Error) -> PluginError {
    if error.is_timeout() {
        PluginError::new(PluginErrorKind::Timeout, "plugin host request timed out")
    } else if error.is_redirect() {
        PluginError::new(
            PluginErrorKind::Incompatible,
            "plugin host attempted a redirect, which is never followed",
        )
    } else {
        PluginError::new(
            PluginErrorKind::HostUnavailable,
            "plugin host connection failed",
        )
    }
}

/// Read at most `limit` bytes, failing rather than truncating.
async fn read_limited(
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

impl HostState {
    fn new(
        config: Arc<PluginHostConfig>,
        shutdown: CancellationToken,
        observability: ObservabilityHandle,
    ) -> Result<Arc<Self>, PluginError> {
        install_crypto_provider();

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(PolicyResolver {
                allow_private: config.allow_private_network,
            }))
            .connect_timeout(config.limits.request_timeout)
            .https_only(!config.allow_insecure_http)
            .user_agent(concat!("remedia-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| {
                PluginError::new(
                    PluginErrorKind::HostFailed,
                    "plugin host HTTP client could not be built",
                )
            })?;

        Ok(Arc::new(Self {
            pending: Arc::new(Semaphore::new(config.limits.max_pending_operations)),
            streams: Arc::new(Semaphore::new(config.limits.max_active_streams)),
            config,
            client,
            manifest: tokio::sync::Mutex::new(None),
            health: Mutex::new(Health::default()),
            stats: HostStats::default(),
            shutdown,
            observability,
        }))
    }

    pub(crate) fn config(&self) -> &PluginHostConfig {
        &self.config
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) fn stats(&self) -> &HostStats {
        &self.stats
    }

    pub(crate) fn registry_shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub(crate) fn observability(&self) -> &ObservabilityHandle {
        &self.observability
    }

    /// IP literals bypass the resolver, so they are checked against the same policy here.
    pub(crate) fn ensure_literal_ip_allowed(&self, url: &url::Url) -> Result<(), PluginError> {
        let literal = match url.host() {
            Some(url::Host::Ipv4(address)) => Some(IpAddr::V4(address)),
            Some(url::Host::Ipv6(address)) => Some(IpAddr::V6(address)),
            _ => None,
        };

        if literal
            .is_some_and(|address| !self.config.allow_private_network && !is_public_ip(address))
        {
            return Err(PluginError::new(
                PluginErrorKind::MediaOriginNotAllowed,
                "address is not permitted for this plugin host",
            ));
        }

        Ok(())
    }

    fn record(&self, result: &Result<(), &PluginError>) {
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match result {
            Ok(()) => {
                health.consecutive_failures = 0;
                health.last_success_unix_ms = Some(unix_ms());
            }
            Err(error) if error.affects_health() => {
                health.consecutive_failures += 1;
                health.last_error = Some(error.label());
                health.last_failure_unix_ms = Some(unix_ms());
            }
            Err(_) => {}
        }
    }

    fn url(&self, path: &str) -> url::Url {
        // The configured endpoint is an origin; the path is one of the fixed v1 routes.
        let mut url = self.config.endpoint.clone();

        url.set_path(path);

        url
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.config.token {
            Some(token) => request.bearer_auth(token.expose()),
            None => request,
        }
    }

    /// Fetch and validate `/.well-known/remedia`. Concurrent callers share one fetch.
    pub(crate) async fn discover(
        &self,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<Arc<Manifest>, PluginError> {
        let mut cached = self.manifest.lock().await;

        if let Some(entry) = cached.as_ref() {
            let fresh = entry.fetched_at.elapsed() < self.config.limits.discovery_ttl;

            if !force && fresh {
                return Ok(Arc::clone(&entry.manifest));
            }
        }

        self.stats.discoveries.fetch_add(1, Ordering::Relaxed);

        let result = self.fetch_manifest(cancel).await;

        self.record(&result.as_ref().map(|_| ()));

        match result {
            Ok(manifest) => {
                let manifest = Arc::new(manifest);

                self.health
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .discovered = true;
                *cached = Some(CachedManifest {
                    manifest: Arc::clone(&manifest),
                    fetched_at: Instant::now(),
                });
                with_dispatch(&self.observability, || {
                    tracing::info!(
                        name: "server.plugin_host.discovered",
                        target: "remedia_server::plugin_host",
                        host_id = %self.config.id,
                        operations = manifest.operation_count() as u64,
                        "plugin host discovery succeeded"
                    );
                });

                Ok(manifest)
            }
            Err(error) => {
                self.stats
                    .discovery_failures
                    .fetch_add(1, Ordering::Relaxed);
                // A host which changed identity or protocol must not keep serving a stale manifest.
                if matches!(
                    error.kind,
                    PluginErrorKind::Incompatible | PluginErrorKind::Unauthorized
                ) {
                    *cached = None;
                }
                with_dispatch(&self.observability, || {
                    tracing::warn!(
                        name: "server.plugin_host.discovery_failed",
                        target: "remedia_server::plugin_host",
                        host_id = %self.config.id,
                        error_kind = error.label(),
                        "plugin host discovery failed"
                    );
                });

                Err(error)
            }
        }
    }

    async fn fetch_manifest(&self, cancel: &CancellationToken) -> Result<Manifest, PluginError> {
        let request = self.authorize(self.client.get(self.url(MANIFEST_PATH)));
        let response = send_cancellable(
            request,
            cancel,
            &self.shutdown,
            self.config.limits.request_timeout,
        )
        .await?;
        let status = response.status();

        if !status.is_success() {
            let body = read_limited(
                response,
                4_096,
                cancel,
                &self.shutdown,
                self.config.limits.request_timeout,
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
            self.config.limits.max_response_bytes,
            cancel,
            &self.shutdown,
            self.config.limits.request_timeout,
        )
        .await?;

        validate_manifest(&self.config, &body)
    }

    /// Verify the operation is allowed and currently advertised, refreshing a stale view once.
    async fn ensure_operation(
        &self,
        plugin_id: &str,
        operation: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PluginError> {
        let manifest = self.discover(false, cancel).await?;

        if manifest.advertises(plugin_id, operation) {
            return Ok(());
        }

        let stale = {
            let cached = self.manifest.lock().await;

            cached
                .as_ref()
                .is_none_or(|entry| entry.fetched_at.elapsed() >= REDISCOVERY_FLOOR)
        };

        if stale
            && self
                .discover(true, cancel)
                .await?
                .advertises(plugin_id, operation)
        {
            return Ok(());
        }

        Err(PluginError::new(
            PluginErrorKind::OperationUnavailable,
            "plugin host does not advertise the requested operation",
        ))
    }

    /// One authenticated execute request. Bounded in size, time and memory.
    async fn execute_once(
        &self,
        operation: &OperationRef<'_>,
        cancel: &CancellationToken,
    ) -> Result<ExecuteOutcome, PluginError> {
        let body = serde_json::to_vec(&json!({
            "pluginId": operation.plugin_id,
            "operation": operation.operation,
            "input": operation.input,
        }))
        .map_err(|_| {
            PluginError::new(
                PluginErrorKind::InvalidRequest,
                "operation input could not be serialized",
            )
        })?;

        if body.len() > self.config.limits.max_request_bytes {
            return Err(PluginError::new(
                PluginErrorKind::RequestTooLarge,
                "operation input exceeds the configured request limit",
            ));
        }

        let request = self
            .authorize(self.client.post(self.url(EXECUTE_PATH)))
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("x-remedia-operation-id", operation_id())
            .body(body);
        let timeout = self.config.limits.request_timeout;
        let response = send_cancellable(request, cancel, &self.shutdown, timeout).await?;
        let status = response.status();

        if status.is_success() {
            let body = read_limited(
                response,
                self.config.limits.max_response_bytes,
                cancel,
                &self.shutdown,
                timeout,
            )
            .await?;

            return parse_execute_response(&body);
        }

        let body = read_limited(response, 4_096, cancel, &self.shutdown, timeout)
            .await
            .unwrap_or_default();

        Err(error_from_status(
            status.as_u16(),
            error_code(&body).as_deref(),
        ))
    }

    fn acquire(&self, semaphore: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, PluginError> {
        Arc::clone(semaphore).try_acquire_owned().map_err(|_| {
            PluginError::new(
                PluginErrorKind::Exhausted,
                "plugin host concurrency limit reached",
            )
        })
    }

    /// Execute the operation on this deployment and return a decoder input for the result.
    ///
    /// The operation is never run on another host. Replay-safe operations are retried
    /// within `maxAttempts` for transient failures and expired grants; authentication
    /// failures and non-replay-safe operations are never retried.
    pub(crate) async fn resolve(
        self: &Arc<Self>,
        server: &ServerConfig,
        operation: &OperationRef<'_>,
        cancel: &CancellationToken,
    ) -> Result<ResolvedMedia, PluginError> {
        let allowed = self
            .config
            .operation(operation.plugin_id, operation.operation)
            .ok_or_else(|| {
                PluginError::new(
                    PluginErrorKind::OperationNotAllowed,
                    "operation is not allowed for this plugin host",
                )
            })?;

        if self.shutdown.is_cancelled() {
            return Err(PluginError::new(
                PluginErrorKind::ShuttingDown,
                "server is shutting down",
            ));
        }

        let _pending = self.acquire(&self.pending)?;

        self.stats.operations.fetch_add(1, Ordering::Relaxed);

        let started = Instant::now();
        let mut attempt = 1;

        loop {
            let result = self.attempt(server, operation, cancel).await;

            self.record(&result.as_ref().map(|_| ()));

            let error = match result {
                Ok(media) => {
                    with_dispatch(&self.observability, || {
                        tracing::info!(
                            name: "server.plugin_host.operation_completed",
                            target: "remedia_server::plugin_host",
                            host_id = %self.config.id,
                            plugin_id = operation.plugin_id,
                            operation = operation.operation,
                            attempts = u64::from(attempt),
                            duration_ms = started.elapsed().as_millis() as u64,
                            "plugin operation resolved media"
                        );
                    });

                    return Ok(media);
                }
                Err(error) => error,
            };

            self.count_failure(&error);

            let refreshable = error.kind == PluginErrorKind::MediaExpired;
            let retry = allowed.replay_safe
                && attempt < self.config.limits.max_attempts
                && (error.retriable() || refreshable);

            with_dispatch(&self.observability, || {
                tracing::warn!(
                    name: "server.plugin_host.operation_failed",
                    target: "remedia_server::plugin_host",
                    host_id = %self.config.id,
                    plugin_id = operation.plugin_id,
                    operation = operation.operation,
                    error_kind = error.label(),
                    attempt = u64::from(attempt),
                    will_retry = retry,
                    "plugin operation failed"
                );
            });

            if !retry {
                return Err(error);
            }

            self.stats.retries.fetch_add(1, Ordering::Relaxed);
            attempt += 1;

            if !refreshable {
                tokio::select! {
                    () = cancel.cancelled() => return Err(PluginError::new(PluginErrorKind::Cancelled, "operation was cancelled")),
                    () = self.shutdown.cancelled() => return Err(PluginError::new(PluginErrorKind::ShuttingDown, "server is shutting down")),
                    () = tokio::time::sleep(REDISCOVERY_RETRY_DELAY) => {}
                }
            }
        }
    }

    fn count_failure(&self, error: &PluginError) {
        let counter = match error.kind {
            PluginErrorKind::Declined => &self.stats.declined,
            PluginErrorKind::Unauthorized => &self.stats.unauthorized,
            PluginErrorKind::Timeout => &self.stats.timeouts,
            PluginErrorKind::Cancelled | PluginErrorKind::ShuttingDown => &self.stats.cancelled,
            PluginErrorKind::MediaExpired => &self.stats.grants_expired,
            _ => &self.stats.operation_failures,
        };

        counter.fetch_add(1, Ordering::Relaxed);
    }

    async fn attempt(
        self: &Arc<Self>,
        server: &ServerConfig,
        operation: &OperationRef<'_>,
        cancel: &CancellationToken,
    ) -> Result<ResolvedMedia, PluginError> {
        self.ensure_operation(operation.plugin_id, operation.operation, cancel)
            .await?;

        let value = match self.execute_once(operation, cancel).await? {
            ExecuteOutcome::Declined => {
                return Err(PluginError::new(
                    PluginErrorKind::Declined,
                    "plugin operation did not handle the request",
                ));
            }
            ExecuteOutcome::Handled(value) => value,
        };
        let descriptor = parse_descriptor(&value)?;

        if descriptor
            .expires_at_ms
            .is_some_and(|expires| expires <= unix_ms())
        {
            return Err(PluginError::new(
                PluginErrorKind::MediaExpired,
                "media descriptor was already expired",
            ));
        }

        if self.config.owns_media_origin(&descriptor.url) {
            let permit = self.acquire(&self.streams)?;
            let (source, guard) = open_media(self, descriptor.url, cancel, permit).await?;

            return Ok(ResolvedMedia {
                source,
                guard: Some(guard),
            });
        }

        if !self.config.allow_external_media {
            return Err(PluginError::new(
                PluginErrorKind::MediaOriginNotAllowed,
                "media origin is not configured for this plugin host",
            ));
        }

        // Externally hosted media follows the server's ordinary URL policy and FFmpeg's
        // seekable HTTP input. It is outside the host's credential and origin boundary.
        let authorized = server
            .authorize_url(descriptor.url.as_str())
            .await
            .map_err(|MediaAccessError::NotAllowed| {
                PluginError::new(
                    PluginErrorKind::MediaOriginNotAllowed,
                    "media url is outside the configured media access policy",
                )
            })?;

        Ok(ResolvedMedia {
            source: InputSource::http_url(authorized),
            guard: None,
        })
    }

    pub(crate) fn snapshot(&self) -> Value {
        let health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stats = &self.stats;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);

        json!({
            "id": self.config.id,
            "status": {
                "state": self.status_locked(&health),
                "consecutiveFailures": health.consecutive_failures,
                "lastError": health.last_error,
                "lastSuccessAtMs": health.last_success_unix_ms,
                "lastFailureAtMs": health.last_failure_unix_ms,
            },
            "operations": {
                "allowed": self.config.operations.len(),
            },
            "resources": {
                "pending": self.config.limits.max_pending_operations - self.pending.available_permits(),
                "maxPending": self.config.limits.max_pending_operations,
                "activeStreams": self.config.limits.max_active_streams - self.streams.available_permits(),
                "maxActiveStreams": self.config.limits.max_active_streams,
            },
            "totals": {
                "operations": load(&stats.operations),
                "operationFailures": load(&stats.operation_failures),
                "retries": load(&stats.retries),
                "declined": load(&stats.declined),
                "unauthorized": load(&stats.unauthorized),
                "timeouts": load(&stats.timeouts),
                "cancelled": load(&stats.cancelled),
                "grantsOpened": load(&stats.grants_opened),
                "grantsExpired": load(&stats.grants_expired),
                "mediaBytes": load(&stats.media_bytes),
                "mediaFailures": load(&stats.media_failures),
                "discoveries": load(&stats.discoveries),
                "discoveryFailures": load(&stats.discovery_failures),
            },
        })
    }

    fn status_locked(&self, health: &Health) -> &'static str {
        if self.shutdown.is_cancelled() {
            "shutting_down"
        } else if health.consecutive_failures >= UNHEALTHY_AFTER {
            "unavailable"
        } else if health.consecutive_failures > 0 {
            "degraded"
        } else if health.discovered || health.last_success_unix_ms.is_some() {
            "healthy"
        } else {
            "unknown"
        }
    }

    pub(crate) fn active_streams(&self) -> usize {
        self.config.limits.max_active_streams - self.streams.available_permits()
    }
}

/// All configured deployments, keyed by their stable host ID.
pub(crate) struct PluginHostRegistry {
    hosts: Vec<Arc<HostState>>,
    shutdown: CancellationToken,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl PluginHostRegistry {
    pub(crate) fn new(
        configs: &[Arc<PluginHostConfig>],
        observability: &ObservabilityHandle,
    ) -> Result<Self, PluginError> {
        let shutdown = CancellationToken::new();
        let mut hosts = Vec::with_capacity(configs.len());

        for config in configs {
            hosts.push(HostState::new(
                Arc::clone(config),
                shutdown.clone(),
                observability.clone(),
            )?);
        }

        Ok(Self {
            hosts,
            shutdown,
            tasks: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn host(&self, id: &str) -> Option<&Arc<HostState>> {
        self.hosts.iter().find(|host| host.config.id == id)
    }

    pub(crate) fn host_ids(&self) -> Vec<&str> {
        self.hosts
            .iter()
            .map(|host| host.config.id.as_str())
            .collect()
    }

    /// Begin background discovery. Failures only affect reported health; hosts may start later.
    pub(crate) fn start(&self) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        for host in &self.hosts {
            let host = Arc::clone(host);
            let shutdown = self.shutdown.clone();
            let observability = host.observability.clone();

            tasks.push(tokio::spawn(
                async move {
                    let discovery = async {
                        loop {
                            let _ = host.discover(true, &shutdown).await;

                            tokio::select! {
                                () = shutdown.cancelled() => break,
                                () = tokio::time::sleep(host.config.limits.discovery_ttl) => {}
                            }
                        }
                    };

                    tokio::select! {
                        () = shutdown.cancelled() => {},
                        () = discovery => {},
                    }
                }
                .with_subscriber(
                    observability
                        .dispatch()
                        .cloned()
                        .unwrap_or_else(tracing::Dispatch::none),
                ),
            ));
        }
    }

    pub(crate) fn snapshot(&self) -> Value {
        Value::Array(self.hosts.iter().map(|host| host.snapshot()).collect())
    }

    /// Capability advertised through `node.info`.
    pub(crate) fn capability(&self) -> Value {
        json!({
            "available": !self.hosts.is_empty(),
            "hosts": self.host_ids(),
        })
    }

    /// Cancel pending operations and media transfers, then wait briefly for them to unwind.
    pub(crate) async fn close(&self) {
        self.shutdown.cancel();

        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );

        for task in tasks {
            let _ = task.await;
        }

        let deadline = Instant::now() + CLOSE_DRAIN;

        while self.hosts.iter().any(|host| host.active_streams() > 0) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Diagnostic probe used by `doctor`: discover every host once.
    pub(crate) async fn diagnose(&self) -> Vec<HostDiagnostic> {
        let cancel = CancellationToken::new();
        let mut results = Vec::with_capacity(self.hosts.len());

        for host in &self.hosts {
            let discovery = host.discover(true, &cancel).await;
            let (state, detail, missing) = match &discovery {
                Ok(manifest) => {
                    let missing = host
                        .config
                        .operations
                        .iter()
                        .filter(|allowed| {
                            !manifest.advertises(&allowed.plugin_id, &allowed.operation)
                        })
                        .map(|allowed| format!("{}/{}", allowed.plugin_id, allowed.operation))
                        .collect::<Vec<_>>();

                    (
                        if missing.is_empty() {
                            "healthy"
                        } else {
                            "degraded"
                        },
                        None,
                        missing,
                    )
                }
                Err(error) => ("unavailable", Some(error.detail.to_owned()), Vec::new()),
            };

            results.push(HostDiagnostic {
                id: host.config.id.clone(),
                origin: host.config.endpoint.origin().ascii_serialization(),
                state,
                detail,
                advertised_operations: discovery
                    .as_ref()
                    .map_or(0, |manifest| manifest.operation_count()),
                missing_operations: missing,
            });
        }

        results
    }
}

/// Result of probing one configured host.
#[derive(Clone, Debug)]
pub struct HostDiagnostic {
    /// Configured host ID.
    pub id: String,
    /// Endpoint origin. Never includes credentials.
    pub origin: String,
    /// `healthy`, `degraded` (allowed operations missing) or `unavailable`.
    pub state: &'static str,
    /// Safe failure description when discovery failed.
    pub detail: Option<String>,
    /// Number of operations the host advertises.
    pub advertised_operations: usize,
    /// Configured operations (`pluginId/operation`) which the host does not advertise.
    pub missing_operations: Vec<String>,
}
