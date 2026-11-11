//! Host registry, discovery, authenticated execution and source resolution.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use remedia_core::InputSource;
use remedia_observability::ObservabilityHandle;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;

use remedia_server_config::error::{PluginError, PluginErrorKind};
use remedia_server_config::protocol::{
    EXECUTE_PATH, ExecuteOutcome, Manifest, error_code, error_from_status, parse_descriptor,
    parse_execute_response,
};
use remedia_server_config::transport::{
    authorize, build_client, endpoint_url, ensure_literal_ip_allowed, fetch_manifest, read_limited,
    send_cancellable,
};
use remedia_server_config::{AllowedOperation, PluginHostConfig};

use super::media::{MediaGuard, open_media};
use crate::config::{MediaAccessError, ServerConfig};

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
    /// Generic (non-media) operations started. Media resolution is counted in `operations`.
    pub(crate) generic_operations: AtomicU64,
    /// Generic operations which failed for a reason other than the shared timeout,
    /// authorization and cancellation categories.
    pub(crate) generic_failures: AtomicU64,
    /// Generic operations the host answered with `handled: false`.
    pub(crate) generic_unhandled: AtomicU64,
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

/// What an operation's handled result is used for. Both purposes share discovery, the
/// allowlist, concurrency limits, retries and health tracking; they differ in how the result
/// is interpreted and which counters record it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Purpose {
    /// The result is a media descriptor which is opened for playback.
    Media,
    /// The result is opaque JSON returned to the caller unchanged.
    Generic,
}

impl Purpose {
    const fn label(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Generic => "generic",
        }
    }
}

/// A host as exposed by `plugin.list`: its stable ID, availability and usable operations.
///
/// Never carries the endpoint, credentials or any other deployment detail.
pub(crate) struct HostListing {
    pub(crate) host_id: String,
    pub(crate) status: &'static str,
    /// Operations which are both allowlisted by the operator and advertised by the host,
    /// sorted by plugin ID and operation name.
    pub(crate) operations: Vec<(String, String)>,
}

impl HostListing {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "hostId": self.host_id,
            "status": self.status,
            "operations": self.operations.iter().map(|(plugin_id, operation)| json!({
                "pluginId": plugin_id,
                "operation": operation,
            })).collect::<Vec<_>>(),
        })
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn operation_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

impl HostState {
    fn new(
        config: Arc<PluginHostConfig>,
        shutdown: CancellationToken,
        observability: ObservabilityHandle,
    ) -> Result<Arc<Self>, PluginError> {
        let client = build_client(&config)?;

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
        ensure_literal_ip_allowed(&self.config, url)
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

        let result = fetch_manifest(&self.client, &self.config, cancel, &self.shutdown).await;

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

        let request = authorize(
            &self.config,
            self.client.post(endpoint_url(&self.config, EXECUTE_PATH)),
        )
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

    /// The allowlist entry for an operation, or a policy failure.
    fn allowed(&self, operation: &OperationRef<'_>) -> Result<&AllowedOperation, PluginError> {
        self.config
            .operation(operation.plugin_id, operation.operation)
            .ok_or_else(|| {
                PluginError::new(
                    PluginErrorKind::OperationNotAllowed,
                    "operation is not allowed for this plugin host",
                )
            })
    }

    /// Run the operation on this deployment and return its JSON outcome without interpreting it.
    ///
    /// This is the generic forwarding path. `handled: false` is an ordinary outcome, not an
    /// error, and any JSON value (including `null`) is a valid handled result. The operation
    /// is never run on another host. Replay-safe operations are retried within `maxAttempts`
    /// for transient failures; authentication failures and non-replay-safe operations are
    /// never retried.
    pub(crate) async fn execute(
        &self,
        operation: &OperationRef<'_>,
        cancel: &CancellationToken,
    ) -> Result<ExecuteOutcome, PluginError> {
        let replay_safe = self.allowed(operation)?.replay_safe;
        let started = Instant::now();
        let (outcome, attempts) = self
            .run(Purpose::Generic, operation, replay_safe, cancel, || {
                self.invoke(operation, cancel)
            })
            .await?;

        if matches!(outcome, ExecuteOutcome::Declined) {
            self.stats.generic_unhandled.fetch_add(1, Ordering::Relaxed);
        }

        self.log_completed(
            Purpose::Generic,
            operation,
            attempts,
            started,
            match outcome {
                ExecuteOutcome::Declined => "unhandled",
                ExecuteOutcome::Handled(_) => "handled",
            },
        );

        Ok(outcome)
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
        let replay_safe = self.allowed(operation)?.replay_safe;
        let started = Instant::now();
        let (media, attempts) = self
            .run(Purpose::Media, operation, replay_safe, cancel, || {
                self.attempt(server, operation, cancel)
            })
            .await?;

        self.log_completed(Purpose::Media, operation, attempts, started, "resolved");

        Ok(media)
    }

    /// Shared admission, counting, health and retry policy for one logical operation.
    ///
    /// `attempt` performs one try and is repeated only for failures that can plausibly
    /// succeed again. Returns the value together with the number of attempts used.
    async fn run<T, F, Fut>(
        &self,
        purpose: Purpose,
        operation: &OperationRef<'_>,
        replay_safe: bool,
        cancel: &CancellationToken,
        mut attempt: F,
    ) -> Result<(T, u32), PluginError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, PluginError>>,
    {
        if self.shutdown.is_cancelled() {
            return Err(PluginError::new(
                PluginErrorKind::ShuttingDown,
                "server is shutting down",
            ));
        }

        // Held until every attempt finishes, and released on any exit including cancellation.
        let _pending = self.acquire(&self.pending)?;

        match purpose {
            Purpose::Media => &self.stats.operations,
            Purpose::Generic => &self.stats.generic_operations,
        }
        .fetch_add(1, Ordering::Relaxed);

        let mut tries = 1;

        loop {
            let result = attempt().await;

            self.record(&result.as_ref().map(|_| ()));

            let error = match result {
                Ok(value) => return Ok((value, tries)),
                Err(error) => error,
            };

            self.count_failure(purpose, &error);

            let refreshable = error.kind == PluginErrorKind::MediaExpired;
            let retry = replay_safe
                && tries < self.config.limits.max_attempts
                && (error.retriable() || refreshable);

            with_dispatch(&self.observability, || {
                tracing::warn!(
                    name: "server.plugin_host.operation_failed",
                    target: "remedia_server::plugin_host",
                    host_id = %self.config.id,
                    plugin_id = operation.plugin_id,
                    operation = operation.operation,
                    purpose = purpose.label(),
                    error_kind = error.label(),
                    attempt = u64::from(tries),
                    will_retry = retry,
                    "plugin operation failed"
                );
            });

            if !retry {
                return Err(error);
            }

            self.stats.retries.fetch_add(1, Ordering::Relaxed);
            tries += 1;

            if !refreshable {
                tokio::select! {
                    () = cancel.cancelled() => return Err(PluginError::new(PluginErrorKind::Cancelled, "operation was cancelled")),
                    () = self.shutdown.cancelled() => return Err(PluginError::new(PluginErrorKind::ShuttingDown, "server is shutting down")),
                    () = tokio::time::sleep(REDISCOVERY_RETRY_DELAY) => {}
                }
            }
        }
    }

    /// Operation payloads and results are never logged; only low-cardinality identifiers.
    fn log_completed(
        &self,
        purpose: Purpose,
        operation: &OperationRef<'_>,
        attempts: u32,
        started: Instant,
        outcome: &'static str,
    ) {
        with_dispatch(&self.observability, || {
            tracing::info!(
                name: "server.plugin_host.operation_completed",
                target: "remedia_server::plugin_host",
                host_id = %self.config.id,
                plugin_id = operation.plugin_id,
                operation = operation.operation,
                purpose = purpose.label(),
                outcome,
                attempts = u64::from(attempts),
                retries = u64::from(attempts - 1),
                duration_ms = started.elapsed().as_millis() as u64,
                "plugin operation completed"
            );
        });
    }

    fn count_failure(&self, purpose: Purpose, error: &PluginError) {
        let counter = match error.kind {
            PluginErrorKind::Declined => &self.stats.declined,
            PluginErrorKind::Unauthorized => &self.stats.unauthorized,
            PluginErrorKind::Timeout => &self.stats.timeouts,
            PluginErrorKind::Cancelled | PluginErrorKind::ShuttingDown => &self.stats.cancelled,
            PluginErrorKind::MediaExpired => &self.stats.grants_expired,
            _ => match purpose {
                Purpose::Media => &self.stats.operation_failures,
                Purpose::Generic => &self.stats.generic_failures,
            },
        };

        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Confirm the operation is advertised, then run it once. A host which no longer knows
    /// an advertised operation has changed, so its cached manifest is allowed to expire early.
    async fn invoke(
        &self,
        operation: &OperationRef<'_>,
        cancel: &CancellationToken,
    ) -> Result<ExecuteOutcome, PluginError> {
        self.ensure_operation(operation.plugin_id, operation.operation, cancel)
            .await?;

        let result = self.execute_once(operation, cancel).await;

        if matches!(&result, Err(error) if error.kind == PluginErrorKind::OperationUnavailable) {
            self.expire_manifest().await;
        }

        result
    }

    /// Drop a cached manifest which is older than the rediscovery floor so that the next
    /// request refetches it. Bounded: a manifest fetched moments ago is kept.
    async fn expire_manifest(&self) {
        let mut cached = self.manifest.lock().await;

        if cached
            .as_ref()
            .is_some_and(|entry| entry.fetched_at.elapsed() >= REDISCOVERY_FLOOR)
        {
            *cached = None;
        }
    }

    async fn attempt(
        self: &Arc<Self>,
        server: &ServerConfig,
        operation: &OperationRef<'_>,
        cancel: &CancellationToken,
    ) -> Result<ResolvedMedia, PluginError> {
        let value = match self.invoke(operation, cancel).await? {
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
                "genericOperations": load(&stats.generic_operations),
                "genericFailures": load(&stats.generic_failures),
                "genericUnhandled": load(&stats.generic_unhandled),
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

    /// Current availability label, derived from recent request outcomes.
    fn status(&self) -> &'static str {
        let health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        self.status_locked(&health)
    }

    /// Describe the operations a client may currently invoke on this host.
    ///
    /// Uses the cached manifest while it is fresh and refetches it once it is older than
    /// `discoveryTtlMs`, so a restarted host or changed operation set is picked up without
    /// a request per call. `refresh` additionally forces a refetch, but never more often
    /// than the rediscovery floor. If the manifest cannot be validated or refreshed the host
    /// is reported with its degraded or unavailable status and no operations: a stale view
    /// is never advertised as available. Only cancellation and shutdown are returned as errors.
    pub(crate) async fn describe(
        &self,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<HostListing, PluginError> {
        let force = refresh && {
            let cached = self.manifest.lock().await;

            cached
                .as_ref()
                .is_none_or(|entry| entry.fetched_at.elapsed() >= REDISCOVERY_FLOOR)
        };
        let mut operations = match self.discover(force, cancel).await {
            Ok(manifest) => self
                .config
                .operations
                .iter()
                .filter(|allowed| manifest.advertises(&allowed.plugin_id, &allowed.operation))
                .map(|allowed| (allowed.plugin_id.clone(), allowed.operation.clone()))
                .collect(),
            Err(error)
                if matches!(
                    error.kind,
                    PluginErrorKind::Cancelled | PluginErrorKind::ShuttingDown
                ) =>
            {
                return Err(error);
            }
            Err(_) => Vec::new(),
        };

        operations.sort();

        Ok(HostListing {
            host_id: self.config.id.clone(),
            status: self.status(),
            operations,
        })
    }

    pub(crate) fn active_streams(&self) -> usize {
        self.config.limits.max_active_streams - self.streams.available_permits()
    }
}

/// All configured deployments, keyed by their stable host ID.
pub(crate) struct PluginHostRegistry {
    hosts: Vec<Arc<HostState>>,
    /// IDs of configured hosts with `enabled: false`. They are never registered as executable
    /// and requests naming them fail with `plugin_host_disabled` instead of reaching any host.
    disabled: Vec<String>,
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
        let mut disabled = Vec::new();

        for config in configs {
            if !config.enabled {
                disabled.push(config.id.clone());

                continue;
            }

            hosts.push(HostState::new(
                Arc::clone(config),
                shutdown.clone(),
                observability.clone(),
            )?);
        }

        Ok(Self {
            hosts,
            disabled,
            shutdown,
            tasks: Mutex::new(Vec::new()),
        })
    }

    /// The executable host with this ID, or why there is none: it is disabled or unknown.
    pub(crate) fn host(&self, id: &str) -> Result<&Arc<HostState>, PluginError> {
        if let Some(host) = self.hosts.iter().find(|host| host.config.id == id) {
            return Ok(host);
        }

        if self.disabled.iter().any(|disabled| disabled == id) {
            return Err(PluginError::new(
                PluginErrorKind::HostDisabled,
                "this plugin host is disabled in the node's configuration",
            ));
        }

        Err(PluginError::new(
            PluginErrorKind::HostNotFound,
            "no plugin host with that ID is configured on this node",
        ))
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
    ///
    /// `forwarding.available` tells clients that `plugin.list`, `plugin.execute` and
    /// `plugin.cancel` can be used on this node. Servers which predate generic forwarding
    /// omit the field. It carries no endpoint, credential or operation details.
    pub(crate) fn capability(&self) -> Value {
        json!({
            "available": !self.hosts.is_empty(),
            "hosts": self.host_ids(),
            "forwarding": { "available": !self.hosts.is_empty() },
        })
    }

    /// Every configured host with its status and usable operations, ordered by host ID.
    pub(crate) async fn list(
        &self,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<HostListing>, PluginError> {
        let mut listings = futures_util::future::try_join_all(
            self.hosts.iter().map(|host| host.describe(refresh, cancel)),
        )
        .await?;

        listings.sort_by(|left, right| left.host_id.cmp(&right.host_id));

        Ok(listings)
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
