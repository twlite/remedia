#!/bin/sh
# Supply identical native dependency environment through the calling shell.
set -eu
hybrid_root=$(git rev-parse --show-toplevel)
hybrid_output=${1:-/tmp/remedia-hybrid-ab}
mkdir -p "$hybrid_output/original-source"
git -C "$hybrid_root" archive 55b71e55b38722401dbd6d0011ccf31b0337000c | tar -xf - -C "$hybrid_output/original-source"
cp "$hybrid_root/crates/remedia-core/examples/hybrid_scheduler_benchmark.rs" "$hybrid_output/original-source/crates/remedia-core/examples/"
# Declare the disabled harness cfg, without modifying original playback source.
python3 - "$hybrid_output/original-source/crates/remedia-core/Cargo.toml" <<'PY'
import sys
from pathlib import Path
p = Path(sys.argv[1])
p.write_text(p.read_text().replace('[features]', '[features]\nexperimental-scheduler = []'))
PY
CARGO_TARGET_DIR="$hybrid_output/original-target" cargo build --release --locked \
  --manifest-path "$hybrid_output/original-source/Cargo.toml" -p remedia-core --example hybrid_scheduler_benchmark
CARGO_TARGET_DIR="$hybrid_output/dedicated-target" cargo build --release --locked \
  --manifest-path "$hybrid_root/Cargo.toml" -p remedia-core --example hybrid_scheduler_benchmark
CARGO_TARGET_DIR="$hybrid_output/scheduled-target" cargo build --release --locked \
  --manifest-path "$hybrid_root/Cargo.toml" -p remedia-core --features experimental-scheduler \
  --example hybrid_scheduler_benchmark --example resource_audit_fixtures
