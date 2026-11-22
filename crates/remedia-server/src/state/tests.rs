use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use remedia_observability::{Observability, ObservabilityConfig};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{ServerInner, lock, new_inner};
use crate::config::ServerConfig;
use crate::plugin_host::PluginHostRegistry;

static NEXT_CONFIG: AtomicU64 = AtomicU64::new(1);

fn test_inner() -> (Arc<ServerInner>, PathBuf) {
    let config_dir = std::env::temp_dir().join(format!(
        "remedia-state-test-{}-{}",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&config_dir).unwrap();

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../remedia-core/tests/fixtures")
        .canonicalize()
        .unwrap();
    let config_json = json!({
        "listen": "127.0.0.1:0",
        "users": [{"username": "test", "password": "test"}],
        "sessionResumptionTimeoutSeconds": 3600,
        "media": {
            "allowedFileRoots": [fixture_root],
            "allowedUrlHosts": ["*"],
            "allowHttp": true,
            "blockPrivateNetworks": false
        },
        "pluginHosts": [],
        "audio": {"opus": {"bitrate": 96000, "complexity": 5, "vbr": false}},
        "observability": {"console": false}
    });
    let config_path = config_dir.join("remedia.config.json");
    std::fs::write(&config_path, config_json.to_string()).unwrap();

    let config = ServerConfig::from_file(&config_path).unwrap();
    let observability =
        Observability::new(ObservabilityConfig::default().with_console_enabled(false)).unwrap();
    let plugin_hosts =
        PluginHostRegistry::new(&config.plugin_hosts, &observability.handle()).unwrap();
    (
        new_inner(
            config,
            plugin_hosts,
            observability,
            CancellationToken::new(),
        ),
        config_dir,
    )
}

async fn shutdown_inner(inner: &Arc<ServerInner>) {
    inner.shutdown().await;

    if let Some(mut observability) = lock(&inner.observability_owner).take() {
        observability.shutdown().unwrap();
    }
}

async fn wait_for_expiry_tasks(inner: &ServerInner, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while inner.expiry_tasks_active.load(Ordering::Acquire) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expiry tasks did not reach the expected count");
}

async fn lifecycle_cycle(inner: &Arc<ServerInner>, index: usize) -> Weak<super::ClientSession> {
    let (session, credential) = inner
        .create_client("test", 100, "connection-old", 8)
        .unwrap();
    let weak = Arc::downgrade(&session);
    inner
        .create_guild(&session, format!("guild-{index}"))
        .unwrap();

    inner.detach_client(Arc::clone(&session), "connection-old");
    let (resumed, returned_credential) = inner
        .resume_client(&session.id, &credential, "test", 100, "connection-new")
        .unwrap();
    assert!(Arc::ptr_eq(&session, &resumed));
    assert_eq!(credential, returned_credential);

    inner.close_client(&session.id).await.unwrap();
    drop(resumed);
    drop(session);

    weak
}

#[test]
fn opaque_ids_have_expected_entropy_length_and_hex_format() {
    let id = super::opaque_id(32);
    assert_eq!(id.len(), 64);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(id, super::opaque_id(32));
}

#[tokio::test]
async fn resumed_and_closed_sessions_release_their_expiry_waiter() {
    let (inner, config_dir) = test_inner();
    let weak = lifecycle_cycle(&inner, 0).await;

    wait_for_expiry_tasks(&inner, 0).await;

    assert!(weak.upgrade().is_none());
    assert!(lock(&inner.sessions).is_empty());
    assert_eq!(inner.active_player_count(), 0);

    shutdown_inner(&inner).await;
    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test]
async fn an_old_expiry_cannot_close_a_resumed_session() {
    let (inner, config_dir) = test_inner();
    let (session, credential) = inner
        .create_client("test", 100, "connection-old", 8)
        .unwrap();

    inner.detach_client(Arc::clone(&session), "connection-old");
    let detached_generation = session.expiry_generation.load(Ordering::Acquire);
    inner
        .resume_client(&session.id, &credential, "test", 100, "connection-new")
        .unwrap();

    inner
        .expire_detached_client(Arc::clone(&session), detached_generation)
        .await;

    assert!(session.ensure_open().is_ok());
    assert_eq!(
        inner.list_clients("test", 100)[0]["attached"],
        true,
        "a stale expiry must not detach a resumed client"
    );
    inner.close_client(&session.id).await.unwrap();
    drop(session);
    wait_for_expiry_tasks(&inner, 0).await;

    shutdown_inner(&inner).await;
    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test]
async fn shutdown_joins_cleanup_after_its_rpc_receiver_is_dropped() {
    let (inner, config_dir) = test_inner();
    let gate = CancellationToken::new();
    let cleanup_gate = gate.clone();
    drop(inner.spawn_cleanup(async move {
        cleanup_gate.cancelled().await;
        Ok(())
    }));

    let shutdown_inner = Arc::clone(&inner);
    let shutdown = tokio::spawn(async move { shutdown_inner.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());

    gate.cancel();
    shutdown.await.unwrap();
    assert_eq!(lock(&inner.background_tasks).len(), 0);

    if let Some(mut observability) = lock(&inner.observability_owner).take() {
        observability.shutdown().unwrap();
    }
    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test]
async fn shutdown_cancels_and_joins_detached_session_expiry_tasks() {
    let (inner, config_dir) = test_inner();
    let (session, credential) = inner
        .create_client("test", 100, "connection-old", 8)
        .unwrap();
    inner.detach_client(Arc::clone(&session), "connection-old");
    wait_for_expiry_tasks(&inner, 1).await;

    shutdown_inner(&inner).await;

    assert_eq!(inner.expiry_tasks_active.load(Ordering::Acquire), 0);
    assert!(lock(&inner.sessions).is_empty());

    assert!(matches!(
        inner.create_client("test", 100, "connection-late", 8),
        Err(super::StateError::InvalidState)
    ));
    assert!(matches!(
        inner.resume_client(&session.id, &credential, "test", 100, "connection-late"),
        Err(super::StateError::InvalidState)
    ));

    drop(session);
    assert_eq!(lock(&inner.background_tasks).len(), 0);

    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test]
#[ignore = "resource audit workload; run with --ignored --nocapture"]
async fn resource_audit_6_measures_thousand_session_lifecycle_cycles() {
    const CYCLES: usize = 1_000;
    const REPEATS: usize = 3;

    for repeat in 0..REPEATS {
        let (inner, config_dir) = test_inner();
        let started = Instant::now();
        let mut sessions = Vec::with_capacity(CYCLES);

        for index in 0..CYCLES {
            sessions.push(lifecycle_cycle(&inner, index).await);
        }

        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        let retained_sessions = sessions
            .iter()
            .filter(|session| session.upgrade().is_some())
            .count();
        let pending_expiry_tasks = inner.expiry_tasks_active.load(Ordering::Acquire);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        println!(
            "RESOURCE_AUDIT_6 {{\"repeat\":{repeat},\"cycles\":{CYCLES},\"elapsedMs\":{elapsed_ms:.3},\"retainedSessions\":{retained_sessions},\"pendingExpiryTasks\":{pending_expiry_tasks},\"sessionsInMap\":{},\"activePlayers\":{}}}",
            lock(&inner.sessions).len(),
            inner.active_player_count()
        );

        assert!(lock(&inner.sessions).is_empty());
        assert_eq!(inner.active_player_count(), 0);
        assert_eq!(retained_sessions, 0);
        assert_eq!(pending_expiry_tasks, 0);

        shutdown_inner(&inner).await;
        let _ = std::fs::remove_dir_all(config_dir);
    }
}
