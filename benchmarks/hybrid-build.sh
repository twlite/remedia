#!/bin/sh
# Caller supplies matching native dependency environment for both builds.
set -eu
hybrid_root=$(git rev-parse --show-toplevel)
hybrid_output=${1:-/tmp/remedia-hybrid-reproduction}
hybrid_baseline=55b71e55b38722401dbd6d0011ccf31b0337000c
mkdir -p "$hybrid_output/original-source"
git -C "$hybrid_root" archive "$hybrid_baseline" | tar -xf - -C "$hybrid_output/original-source"
CARGO_TARGET_DIR="$hybrid_output/original-target" cargo build --release --locked \
  --manifest-path "$hybrid_output/original-source/Cargo.toml" -p remedia-core \
  --example concurrent_playback_benchmark --example stream_output_benchmark
CARGO_TARGET_DIR="$hybrid_output/experiment-target" cargo build --release --locked \
  --manifest-path "$hybrid_root/Cargo.toml" -p remedia-core --features experimental-scheduler \
  --example hybrid_scheduler_benchmark --example stream_output_benchmark \
  --example concurrent_playback_benchmark --example resource_audit_fixtures
