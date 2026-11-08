#!/bin/sh
set -eu

REPO="twlite/remedia"
INSTALL_DIR="${REMEDIA_INSTALL_DIR:-$HOME/.remedia}"
VERSION="${REMEDIA_VERSION:-latest}"
WITH_FFMPEG=1

log() {
  printf '[remedia] %s\n' "$*"
}

download_file() {
  url="$1"
  destination="$2"
  # The stderr TTY is still available when the installer is run via curl | sh.
  if [ -t 2 ]; then
    curl -fL --retry 3 --progress-bar -o "$destination" "$url"
  else
    curl -fsSL --retry 3 -o "$destination" "$url"
  fi
}
for arg in "$@"; do
  case "$arg" in
    --no-ffmpeg) WITH_FFMPEG=0 ;;
    --help) printf '%s\n' 'Usage: install.sh [--no-ffmpeg]' 'Environment: REMEDIA_INSTALL_DIR, REMEDIA_VERSION (latest or x.y.z)' ; exit 0 ;;
    *) echo "Unsupported argument: $arg" >&2; exit 2 ;;
  esac
done

command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v tar >/dev/null || { echo "tar is required" >&2; exit 1; }
os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Darwin:arm64) target=aarch64-apple-darwin ;;
  *) echo "Unsupported remedia-server platform: $os/$arch" >&2; exit 1 ;;
esac
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
log "Checking GitHub releases..."
if [ "$VERSION" = latest ]; then
  # GitHub's latest endpoint excludes prereleases. Fall back only on HTTP 404.
  release="https://api.github.com/repos/$REPO/releases/latest"
  status="$(curl -sSL --retry 3 -o "$tmp/release.json" -w '%{http_code}' "$release")"
  case "$status" in
    200) ;;
    404) curl -fsSL --retry 3 "https://api.github.com/repos/$REPO/releases?per_page=100" -o "$tmp/release.json" ;;
    *) echo "GitHub release lookup failed (HTTP $status)" >&2; exit 1 ;;
  esac
  tag="$(grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"' "$tmp/release.json" | head -n 1 | cut -d '"' -f 4)"
  [ -n "$tag" ] || { echo "Cannot resolve latest release" >&2; exit 1; }
else
  case "$VERSION" in
    *[!a-zA-Z0-9.+-]*|'') echo "Invalid version" >&2; exit 2 ;;
  esac
  tag="v${VERSION#v}"
fi
asset="remedia-server-$target.tar.gz"
runtime_asset="remedia-server-runtime-$target"
url="https://github.com/$REPO/releases/download/$tag/$asset"
log "Selected Remedia $tag for $target."
log "Downloading server archive..."
download_file "$url" "$tmp/server.tar.gz"
log "Verifying server archive SHA-256..."
# Verify against GitHub's published asset digest before extracting.
digest="$(curl -fsSL --retry 3 "https://api.github.com/repos/$REPO/releases/tags/$tag" |
  grep -A 18 "\"name\": \"$asset\"" | sed -n 's/.*"digest": *"sha256:\([a-fA-F0-9]*\)".*/\1/p' | head -n 1)"
[ "${#digest}" -eq 64 ] || { echo "Release is missing a SHA-256 digest for $asset" >&2; exit 1; }
if command -v sha256sum >/dev/null; then
  actual="$(sha256sum "$tmp/server.tar.gz" | cut -d ' ' -f 1)"
else
  actual="$(shasum -a 256 "$tmp/server.tar.gz" | cut -d ' ' -f 1)"
fi
[ "$actual" = "$digest" ] || { echo "Release checksum mismatch" >&2; exit 1; }
log "Checking archive contents..."
tar -tzf "$tmp/server.tar.gz" | while IFS= read -r item; do
  case "$item" in "remedia-server-$target"|"remedia-server-runtime-$target"|LICENSE|THIRD_PARTY_NOTICES.md|THIRD_PARTY_LICENSES.txt) ;; *) echo "Unexpected archive entry $item" >&2; exit 1 ;; esac
done
log "Extracting server archive..."
mkdir -p "$INSTALL_DIR/bin" "$INSTALL_DIR/libexec" "$INSTALL_DIR/licenses"
tar -xzf "$tmp/server.tar.gz" -C "$tmp"
log "Installing Remedia binaries and commands..."
# Never truncate a currently running executable. Rename a fully written binary instead.
staged="$INSTALL_DIR/libexec/.remedia-server-$.new"
runtime_staged="$INSTALL_DIR/libexec/.remedia-server-runtime-$.new"
install -m 755 "$tmp/remedia-server-$target" "$staged"
install -m 755 "$tmp/$runtime_asset" "$runtime_staged"
mv -f "$runtime_staged" "$INSTALL_DIR/libexec/remedia-server-runtime"
mv -f "$staged" "$INSTALL_DIR/libexec/remedia-server"
for item in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt; do
  cp "$tmp/$item" "$INSTALL_DIR/licenses/$item"
done

if [ "$WITH_FFMPEG" -eq 1 ]; then
  log "Preparing FFmpeg 9 dependencies..."
  if ! command -v micromamba >/dev/null 2>&1 && [ ! -x "$INSTALL_DIR/bin/micromamba" ]; then
    case "$os:$arch" in
      Linux:x86_64) mm=linux-64 ;; Linux:*) mm=linux-aarch64 ;;
      Darwin:x86_64) mm=osx-64 ;; Darwin:*) mm=osx-arm64 ;;
    esac
    log "Downloading micromamba..."
    download_file "https://micro.mamba.pm/api/micromamba/$mm/latest" "$tmp/micromamba.tar.bz2"
    log "Extracting micromamba..."
    tar -xjf "$tmp/micromamba.tar.bz2" -C "$tmp" bin/micromamba
    install -m 755 "$tmp/bin/micromamba" "$INSTALL_DIR/bin/micromamba"
  fi
  if command -v micromamba >/dev/null 2>&1; then MAMBA="$(command -v micromamba)"; else MAMBA="$INSTALL_DIR/bin/micromamba"; fi
  log "Provisioning FFmpeg 9 via micromamba (package resolution may take a while)..."
  if [ -d "$INSTALL_DIR/ffmpeg9/conda-meta" ]; then
    "$MAMBA" install --yes --prefix "$INSTALL_DIR/ffmpeg9" --override-channels -c conda-forge 'ffmpeg=9.0.2'
  else
    "$MAMBA" create --yes --prefix "$INSTALL_DIR/ffmpeg9" --override-channels -c conda-forge 'ffmpeg=9.0.2'
  fi
  test -f "$INSTALL_DIR/ffmpeg9/lib/libavcodec.so.63" || test -f "$INSTALL_DIR/ffmpeg9/lib/libavcodec.63.dylib" || { echo "FFmpeg 9 runtime missing" >&2; exit 1; }
else
  log "Skipping FFmpeg installation (--no-ffmpeg)."
fi

cat > "$INSTALL_DIR/bin/remedia" <<'EOF'
#!/bin/sh
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
if [ -d "$root/ffmpeg9/lib" ]; then
  case "$(uname -s)" in
    Darwin) export DYLD_LIBRARY_PATH="$root/ffmpeg9/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" ;;
    *) export LD_LIBRARY_PATH="$root/ffmpeg9/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
  esac
fi
exec "$root/libexec/remedia-server" "$@"
EOF
chmod +x "$INSTALL_DIR/bin/remedia"
cp "$INSTALL_DIR/bin/remedia" "$INSTALL_DIR/bin/remedia-server"
log "Installed: $INSTALL_DIR/bin/remedia"
log "Add $INSTALL_DIR/bin to PATH, then run: remedia --version"
