#!/bin/sh
# Diagnostic-only build. Never substitute its CPU timings for the uninstrumented A/B rows.
set -eu
hybrid_profile_root=$(git rev-parse --show-toplevel)
hybrid_profile_output=${1:-/tmp/remedia-hybrid-profile}
hybrid_profile_revision=${2:-2be796c}
mkdir -p "$hybrid_profile_output/source"
git -C "$hybrid_profile_root" archive "$hybrid_profile_revision" | tar -xf - -C "$hybrid_profile_output/source"
python3 - "$hybrid_profile_output/source/crates/remedia-core/src/playback/scheduler.rs" <<'PY'
import sys
from pathlib import Path
p = Path(sys.argv[1])
s = p.read_text()
fields = ['ACTION_NS', 'ACTION_MAX_NS', 'ACTION_COUNT', 'CLEANUP_NS', 'CLEANUP_MAX_NS', 'CLEANUP_COUNT']
s = '\n'.join(f'static PROFILE_{f}: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);' for f in fields) + '\n' + s.replace('//! Fixed-size FIFO executor. Admission bounds both live tasks and readiness storage.', '// Diagnostic scheduler copy with action/drop wall-time instrumentation.', 1)
needle = '            let outcome = machine.as_mut().expect("live task")();'
assert s.count(needle) == 1
s = s.replace(needle, '''            let profile_started = Instant::now();
            let outcome = machine.as_mut().expect("live task")();
            let profile_ns = profile_started.elapsed().as_nanos() as u64;
            PROFILE_ACTION_NS.fetch_add(profile_ns, Ordering::Relaxed);
            PROFILE_ACTION_MAX_NS.fetch_max(profile_ns, Ordering::Relaxed);
            PROFILE_ACTION_COUNT.fetch_add(1, Ordering::Relaxed);''')
needle = '            machine.take();'
assert s.count(needle) == 1
s = s.replace(needle, '''            let profile_started = Instant::now();
            machine.take();
            let profile_ns = profile_started.elapsed().as_nanos() as u64;
            PROFILE_CLEANUP_NS.fetch_add(profile_ns, Ordering::Relaxed);
            PROFILE_CLEANUP_MAX_NS.fetch_max(profile_ns, Ordering::Relaxed);
            PROFILE_CLEANUP_COUNT.fetch_add(1, Ordering::Relaxed);''')
a = s.index('impl Drop for PlaybackScheduler')
b = s.index('type Machine', a)
block = s[a:b]
index = block.rfind('\n    }')
assert index >= 0
report = '\n        eprintln!("HYBRID_TIMING action_ns={} action_max_ns={} actions={} cleanup_ns={} cleanup_max_ns={} cleanups={}", '+', '.join(f'PROFILE_{f}.load(Ordering::Relaxed)' for f in ['ACTION_NS','ACTION_MAX_NS','ACTION_COUNT','CLEANUP_NS','CLEANUP_MAX_NS','CLEANUP_COUNT'])+');'
block = block[:index] + report + block[index:]
s = s[:a] + block + s[b:]
p.write_text(s)
PY
CARGO_TARGET_DIR="$hybrid_profile_output/target" cargo build --release --locked \
  --manifest-path "$hybrid_profile_output/source/Cargo.toml" -p remedia-core \
  --features experimental-scheduler --example hybrid_scheduler_benchmark
