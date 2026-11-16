#!/usr/bin/env bash
# Capture only target-specific generated configuration. Dependency trees are shared snapshots.
set -euo pipefail
target="${RUST_TARGET:?}"
work_root="${RUNNER_TEMP:?}"
if command -v cygpath >/dev/null 2>&1; then work_root="$(cygpath -u "$work_root")"; fi
prefix="${REMEDIA_STATIC_FFMPEG_DIR:?}"
if command -v cygpath >/dev/null 2>&1; then prefix="$(cygpath -u "$prefix")"; fi
staging="$work_root/native-source-$target"
mkdir -p "$staging"
cp -R "$prefix/share/remedia-build/." "$staging/"
rustc -vV > "$staging/rustc.txt"
if [[ "$target" == *windows* ]]; then
  printf 'MSVC=%s\nWindowsSDK=%s\n' "${VCToolsVersion:-}" "${WindowsSDKVersion:-}" > "$staging/compiler.txt"
else
  "${CC:-cc}" --version > "$staging/compiler.txt"
fi
# The config audit is also performed on cache hits by the dependency build recipe.
test -s "$staging/config.h"
test -s "$staging/config.mak"
python=python3
if ! command -v "$python" >/dev/null 2>&1; then python=python; fi
"$python" "$(dirname "$0")/source-archive.py" "$staging" "$work_root/native-source-$target.tar.gz"
