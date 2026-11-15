#!/usr/bin/env bash
# Native POSIX recipe. Cross targets share the same source and licensing policy.
set -euo pipefail
work="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/remedia-static"
prefix="${FFMPEG_DIR:-$work/install}"
mkdir -p "$work"
fetch_source() {
  local name="$1" url="$2" revision="$3"
  if [[ ! -d "$work/$name/.git" ]]; then
    git init -q "$work/$name"
    git -C "$work/$name" remote add origin "$url"
  fi
  git -C "$work/$name" fetch -q --depth=1 origin "$revision"
  git -C "$work/$name" checkout -q --detach "$revision"
  test "$(git -C "$work/$name" rev-parse HEAD)" = "$revision"
  test -z "$(git -C "$work/$name" status --porcelain)"
}
fetch_source ffmpeg https://github.com/FFmpeg/FFmpeg.git 946fcce07b6dcd0331c8cc609192aeff5e1924f8
export SOURCE_DATE_EPOCH="$(git -C "$work/ffmpeg" show -s --format=%ct HEAD)"
args=(--prefix="$prefix" --enable-static --disable-shared --enable-gpl --enable-version3
  --disable-autodetect --disable-programs --disable-doc --disable-debug --enable-pic
  --enable-network --disable-avdevice)
case "$(uname -s)" in
  Darwin) args+=(--enable-securetransport) ;;
  *)
    fetch_source openssl https://github.com/openssl/openssl.git 7b371d80d959ec9ab4139d09d78e83c090de9779
    (cd "$work/openssl"; ./config --prefix="$prefix" --libdir=lib --openssldir=/etc/ssl no-shared no-tests no-module -fPIC; "${MAKE:-make}" -j"${FFMPEG_BUILD_JOBS:-4}"; "${MAKE:-make}" install_sw)
    export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
    args+=(--enable-openssl --extra-cflags="-I$prefix/include" --extra-ldflags="-L$prefix/lib")
    ;;
esac
while IFS= read -r option; do args+=("$option"); done < "$(dirname "$0")/../native-deps/ffmpeg-audio-options.txt"
(cd "$work/ffmpeg"; if [[ -f ffbuild/config.mak ]]; then "${MAKE:-make}" clean; fi; ./configure "${args[@]}"; "${MAKE:-make}" -j"${FFMPEG_BUILD_JOBS:-4}"; "${MAKE:-make}" install)
node "$(dirname "$0")/audit-ffmpeg-config.mjs" "$work/ffmpeg/ffbuild/config.mak"
mkdir -p "$prefix/share/remedia-build"
cp "$work/ffmpeg/ffbuild/config.mak" "$work/ffmpeg/config.h" "$prefix/share/remedia-build/"
printf '%s\n' "${args[@]}" > "$prefix/share/remedia-build/configure-arguments.txt"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  printf 'REMEDIA_STATIC_FFMPEG_DIR=%s\nPKG_CONFIG_PATH=%s/lib/pkgconfig\n' "$prefix" "$prefix" >> "$GITHUB_ENV"
fi
