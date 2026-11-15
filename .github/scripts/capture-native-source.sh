#!/usr/bin/env bash
set -euo pipefail
target="${RUST_TARGET:?}"
staging="$RUNNER_TEMP/native-source-$target"
mkdir -p "$staging"
# Includes pinned source, license texts, generated configuration, and vcpkg patches.
if [[ "$target" == *windows* ]]; then
  config_file="$(find "$RUNNER_TEMP/remedia-vcpkg/buildtrees/ffmpeg" -name config.h -print -quit)"
  test -n "$config_file"
  grep -q '^#define CONFIG_GPL 1' "$config_file"
  grep -q '^#define CONFIG_VERSION3 1' "$config_file"
  grep -q '^#define CONFIG_NONFREE 0' "$config_file"
  if grep -Eq '^#define CONFIG_LIB[A-Z0-9_]+ 1$' "$config_file"; then
    echo 'Unaudited external FFmpeg library enabled in Windows build' >&2
    exit 1
  fi
  mkdir -p "$staging/vcpkg-source"
  tar --exclude=.git --exclude=buildtrees --exclude=downloads --exclude=packages --exclude=installed -cf - -C "$RUNNER_TEMP/remedia-vcpkg" . | tar -xf - -C "$staging/vcpkg-source"
  cp -R "$RUNNER_TEMP/remedia-vcpkg/ports/ffmpeg" "$staging/ffmpeg-port"
  cp -R "$RUNNER_TEMP/remedia-vcpkg/buildtrees/ffmpeg" "$staging/ffmpeg-build"
  cp -R "$RUNNER_TEMP/remedia-vcpkg/downloads" "$staging/downloads"
else
  for source in "$RUNNER_TEMP/remedia-static/ffmpeg" "$RUNNER_TEMP/remedia-static/openssl" "$RUNNER_TEMP/remedia-ffmpeg-source-$target" "$RUNNER_TEMP/remedia-ffmpeg-source-$target-openssl"; do
    if [[ -d "$source" ]]; then
      mkdir -p "$staging/$(basename "$source")"
      # Exclude generated object archives, retain all source and build records.
      tar --exclude=.git --exclude='*.o' --exclude='*.a' -cf - -C "$source" . | tar -xf - -C "$staging/$(basename "$source")"
    fi
  done
fi
find "$staging" -type f -print -quit | grep -q .
tar -czf "$RUNNER_TEMP/native-source-$target.tar.gz" -C "$staging" .
