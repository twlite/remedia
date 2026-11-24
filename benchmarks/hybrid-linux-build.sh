#!/bin/sh
# Build all variants against the same pinned native recipe in an isolated Linux container.
# Run from the repository; requires Docker and network access for toolchain/native sources.
set -eu
hybrid_root=$(git rev-parse --show-toplevel)
hybrid_output=${1:-/tmp/remedia-hybrid-linux}
hybrid_container=${HYBRID_CONTAINER:-remedia-hybrid-linux}
mkdir -p "$hybrid_output"
if ! docker container inspect "$hybrid_container" >/dev/null 2>&1; then
  docker run -d --name "$hybrid_container" --dns "${HYBRID_DNS:-1.1.1.1}" --cpus 4 \
    -v "$hybrid_output:/experiment" -v "$hybrid_root:/repo:ro" ubuntu:24.04 sleep infinity
fi
docker start "$hybrid_container" >/dev/null
docker exec -e HYBRID_LINUX_RELEASE_DIR="${HYBRID_LINUX_RELEASE_DIR:-/experiment/releases}" "$hybrid_container" bash -c '
  set -e
  if ! command -v clang >/dev/null || ! command -v node >/dev/null; then
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends build-essential ca-certificates clang cmake curl git nasm nodejs perl pkg-config python3
  fi
  if [ ! -f /root/.cargo/env ]; then
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.99.1 --profile minimal
  fi
  . /root/.cargo/env
  git config --global --add safe.directory /repo
  cd /repo
  export RUNNER_TEMP=/experiment/native-work FFMPEG_DIR=/experiment/native-prefix FFMPEG_BUILD_JOBS=4
  bash .github/scripts/build-ffmpeg-static.sh
  export PKG_CONFIG_PATH=/experiment/native-prefix/lib/pkgconfig CARGO_BUILD_JOBS=4 HYBRID_COPY_BUILD_CACHE=1
  bash benchmarks/hybrid-ab-build.sh "$HYBRID_LINUX_RELEASE_DIR"
  CARGO_TARGET_DIR="$HYBRID_LINUX_RELEASE_DIR/scheduled-target" cargo test --release --locked -p remedia-discord --features experimental-scheduler --lib --no-run
  CARGO_TARGET_DIR="$HYBRID_LINUX_RELEASE_DIR/scheduled-target" cargo test --release --locked -p remedia-core --features experimental-scheduler --test experimental_scheduler
'
# Before each measurement series select an explicit quota AND affinity, e.g.:
# docker update --cpus 2 --cpuset-cpus 0,1 "$hybrid_container"
# docker exec "$hybrid_container" python3 /repo/benchmarks/hybrid_ab.py ...
# Keep output paths fresh. Stop the container after collecting results.
