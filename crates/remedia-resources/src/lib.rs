//! Native process measurements. Unsupported values remain absent rather than
//! substituting a different accounting model. Collection never shells out.
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Process memory in bytes. These independently sampled counters are not additive.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySnapshot {
    /// Resident pages, including shared pages.
    pub resident_bytes: Option<u64>,
    /// Private resident clean + dirty pages (USS), not private commit.
    pub private_resident_bytes: Option<u64>,
    /// Resident pages with shared pages proportionally attributed (PSS).
    pub proportional_resident_bytes: Option<u64>,
    /// Private committed bytes, including potentially nonresident pages.
    pub committed_private_bytes: Option<u64>,
    /// Total mapped virtual address space.
    pub virtual_bytes: Option<u64>,
    /// Reserved, uncommitted virtual regions; not physical memory.
    pub reserved_virtual_bytes: Option<u64>,
    /// Lifetime peak resident size.
    pub peak_resident_bytes: Option<u64>,
    /// macOS physical footprint; distinct from USS and RSS.
    pub footprint_bytes: Option<u64>,
}

/// CPU accounting for the whole process, including other embedded engines.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuSnapshot {
    /// 100% means one logical CPU; may exceed 100. First sample is null.
    pub process_percent: Option<f64>,
    /// Cumulative user CPU time in milliseconds.
    pub user_time_ms: Option<f64>,
    /// Cumulative system CPU time in milliseconds.
    pub system_time_ms: Option<f64>,
    /// Rust's available parallelism estimate, respecting platform constraints.
    pub available_parallelism: Option<usize>,
    /// Actual OS thread count, not number of audio players.
    pub thread_count: Option<u64>,
    /// System-wide one minute load average, not process CPU consumption.
    pub load_average_1m: Option<f64>,
}

/// Cached measurements for the shared host process. Null counters are unsupported
/// or unavailable; an absent timestamp means collection has not completed yet.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
    pub memory: MemorySnapshot,
    pub cpu: CpuSnapshot,
    /// Unix milliseconds when collection finished.
    pub collected_at_unix_ms: Option<u64>,
    /// Wall time between samples used for CPU utilization, in milliseconds.
    pub cpu_sample_interval_ms: Option<f64>,
    /// Cache refresh threshold in milliseconds; refresh is demand driven.
    pub sampling_interval_ms: u64,
    /// Collection wall time, including native system interfaces.
    pub collection_duration_ms: Option<f64>,
    /// Unix descriptors or Windows handles currently allocated by the process.
    pub open_handles: Option<u64>,
}

const INTERVAL: Duration = Duration::from_secs(1);

struct Cache {
    snapshot: ProcessSnapshot,
    sampled: Option<Instant>,
    refreshing: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            snapshot: ProcessSnapshot {
                sampling_interval_ms: 1000,
                ..Default::default()
            },
            sampled: None,
            refreshing: false,
        }
    }
}

static CACHE: OnceLock<Arc<Mutex<Cache>>> = OnceLock::new();

/// Return the latest snapshot immediately. At most one short-lived collector
/// refreshes it off the calling thread after the one second interval expires.
/// Concurrent calls share stale data while refreshing; no Tokio worker runs OS I/O.
pub fn cached_snapshot() -> ProcessSnapshot {
    let cache = CACHE.get_or_init(|| Arc::new(Mutex::new(Cache::default())));
    let mut state = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = state.snapshot.clone();
    if state.refreshing || state.sampled.is_some_and(|at| at.elapsed() < INTERVAL) {
        return result;
    }
    state.refreshing = true;
    drop(state);
    let worker_cache = Arc::clone(cache);
    if std::thread::Builder::new()
        .name("remedia-resources".into())
        .spawn(move || {
            // OS failures produce null fields, without logging process data or secrets.
            let mut snapshot = collect();
            let now = Instant::now();
            let mut state = worker_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(previous) = state.sampled {
                let elapsed_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
                snapshot.cpu_sample_interval_ms = Some(elapsed_ms);
                snapshot.cpu.process_percent =
                    cpu_percent(&state.snapshot.cpu, &snapshot.cpu, elapsed_ms);
            }
            state.snapshot = snapshot;
            state.sampled = Some(now);
            state.refreshing = false;
        })
        .is_err()
    {
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refreshing = false;
    }
    result
}

fn cpu_percent(before: &CpuSnapshot, after: &CpuSnapshot, elapsed_ms: f64) -> Option<f64> {
    let delta = after.user_time_ms? + after.system_time_ms?
        - before.user_time_ms?
        - before.system_time_ms?;
    (elapsed_ms > 0.0 && delta >= 0.0).then_some(100.0 * delta / elapsed_ms)
}

/// Collect immediately on a blocking caller. Prefer [`cached_snapshot`] in RPC,
/// async, or latency-sensitive code. CPU percent needs two cached samples.
pub fn collect() -> ProcessSnapshot {
    let started = Instant::now();
    let mut result = ProcessSnapshot {
        sampling_interval_ms: 1000,
        ..Default::default()
    };
    result.cpu.available_parallelism = std::thread::available_parallelism().ok().map(usize::from);
    platform::collect(&mut result);
    result.collected_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| u64::try_from(v.as_millis()).ok());
    result.collection_duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    result
}

#[cfg(unix)]
fn unix_usage(result: &mut ProcessSnapshot) {
    // SAFETY: getrusage writes the valid, zero-initialized structure synchronously.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
        result.cpu.user_time_ms =
            Some(usage.ru_utime.tv_sec as f64 * 1000.0 + usage.ru_utime.tv_usec as f64 / 1000.0);
        result.cpu.system_time_ms =
            Some(usage.ru_stime.tv_sec as f64 * 1000.0 + usage.ru_stime.tv_usec as f64 / 1000.0);
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    let mut load = 0.0;
    // SAFETY: one writable f64 matches the requested number of load averages.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    if unsafe { libc::getloadavg(&mut load, 1) } == 1 {
        result.cpu.load_average_1m = Some(load);
    }
}

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    pub fn collect(result: &mut super::ProcessSnapshot) {
        #[cfg(unix)]
        super::unix_usage(result);
        #[cfg(not(unix))]
        let _ = result;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_percent_counts_logical_cpu_time_without_normalizing() {
        let before = CpuSnapshot {
            user_time_ms: Some(100.0),
            system_time_ms: Some(20.0),
            ..Default::default()
        };
        let after = CpuSnapshot {
            user_time_ms: Some(280.0),
            system_time_ms: Some(40.0),
            ..Default::default()
        };
        assert_eq!(cpu_percent(&before, &after, 100.0), Some(200.0));
        assert_eq!(cpu_percent(&before, &after, 0.0), None);
        assert_eq!(cpu_percent(&after, &before, 100.0), None);
        assert_eq!(cpu_percent(&CpuSnapshot::default(), &after, 100.0), None);
    }

    #[test]
    fn concurrent_cache_reads_share_one_completed_snapshot() {
        let _ = cached_snapshot();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if cached_snapshot().collected_at_unix_ms.is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let workers: Vec<_> = (0..16)
            .map(|_| std::thread::spawn(cached_snapshot))
            .collect();
        let samples: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(
            samples
                .windows(2)
                .all(|s| s[0].collected_at_unix_ms == s[1].collected_at_unix_ms)
        );
        assert!(samples[0].cpu.process_percent.is_none());
    }
}

#[cfg(all(test, not(target_os = "linux")))]
#[path = "linux.rs"]
mod linux_parser_tests;
