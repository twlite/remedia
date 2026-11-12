#!/bin/sh
set -eu

REPO="twlite/remedia"
INSTALL_DIR="${REMEDIA_INSTALL_DIR:-$HOME/.remedia}"
VERSION="${REMEDIA_VERSION:-latest}"
WITH_FFMPEG=1
WITH_SERVER=1

# FFmpeg 9 shared-library ABI majors (not application versions). Keep in sync with
# .github/scripts/ffmpeg-abi.mjs and packages/remedia-node/lib/ffmpeg-runtime.ts.
FFMPEG_ABI="avcodec:63 avformat:63 avutil:61 avfilter:12 swresample:7"
FFMPEG_PACKAGE="ffmpeg=9.0.2"

log() {
  printf '[remedia] %s\n' "$*"
}

usage() {
  cat <<'EOF'
Usage: install.sh [--ffmpeg-only | --no-ffmpeg]

  (default)       Install the Remedia server and FFmpeg 9.
  --ffmpeg-only   Install or verify only FFmpeg 9 under <install dir>/ffmpeg9. No server
                  archive is downloaded and no commands, wrappers or PATH entries are created.
  --no-ffmpeg     Install only the Remedia server and skip FFmpeg.

Environment:
  REMEDIA_INSTALL_DIR   Installation prefix (default: ~/.remedia)
  REMEDIA_VERSION       Server release, latest or x.y.z (ignored by --ffmpeg-only)
EOF
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

configure_shell_path() {
  path_dir="$INSTALL_DIR/bin"
  case "${SHELL:-}" in
    */bash|bash) profile_path="$HOME/.bashrc" ;;
    */zsh|zsh) profile_path="$HOME/.zshrc" ;;
    *) profile_path="$HOME/.profile" ;;
  esac

  # Single-quote the install directory for safe inclusion in shell startup files.
  escaped_dir="$(printf '%s' "$path_dir" | sed "s/'/'\\\\''/g")"
  shell_dir="'$escaped_dir'"
  marker="# Remedia installer PATH: $shell_dir"
  if [ ! -f "$profile_path" ] || ! grep -Fqx "$marker" "$profile_path"; then
    {
      printf '\n%s\n' "$marker"
      printf '%s\n' 'case ":$PATH:" in'
      printf '  *:%s:*) ;;\n' "$shell_dir"
      printf '  *) export PATH=%s:"$PATH" ;;\n' "$shell_dir"
      printf '%s\n' 'esac'
    } >> "$profile_path"
    log "Added $path_dir to PATH in $profile_path."
  else
    log "Remedia PATH entry already exists in $profile_path."
  fi
  # A piped installer cannot modify its parent shell.
  case ":$PATH:" in
    *:"$path_dir":*) ;;
    *) PATH="$path_dir:$PATH"; export PATH ;;
  esac
}

# Resolve the micromamba executable into MAMBA. An existing installation is reused as is.
# `persist` decides where a missing binary is placed: "bin" keeps it in $INSTALL_DIR/bin for later
# runs (server installs); anything else uses the temporary directory so that --ffmpeg-only does
# not create server directories.
ensure_micromamba() {
  persist="$1"
  if command -v micromamba >/dev/null 2>&1; then
    MAMBA="$(command -v micromamba)"
    return 0
  fi
  if [ -x "$INSTALL_DIR/bin/micromamba" ]; then
    MAMBA="$INSTALL_DIR/bin/micromamba"
    return 0
  fi
  case "$os:$arch" in
    Linux:x86_64) mm=linux-64 ;; Linux:*) mm=linux-aarch64 ;;
    Darwin:x86_64) mm=osx-64 ;; Darwin:*) mm=osx-arm64 ;;
  esac
  log "Downloading micromamba..."
  download_file "https://micro.mamba.pm/api/micromamba/$mm/latest" "$tmp/micromamba.tar.bz2"
  log "Extracting micromamba..."
  tar -xjf "$tmp/micromamba.tar.bz2" -C "$tmp" bin/micromamba
  if [ "$persist" = bin ]; then
    mkdir -p "$INSTALL_DIR/bin"
    install -m 755 "$tmp/bin/micromamba" "$INSTALL_DIR/bin/micromamba"
    MAMBA="$INSTALL_DIR/bin/micromamba"
  else
    MAMBA="$tmp/bin/micromamba"
  fi
}

# Install or update FFmpeg 9 and its shared libraries in $INSTALL_DIR/ffmpeg9.
provision_ffmpeg() {
  persist="$1"
  FFMPEG_PREFIX="$INSTALL_DIR/ffmpeg9"
  log "Preparing FFmpeg 9 dependencies..."
  mkdir -p "$INSTALL_DIR"
  ensure_micromamba "$persist"
  log "Provisioning FFmpeg 9 via micromamba (package resolution may take a while)..."
  if [ -d "$FFMPEG_PREFIX/conda-meta" ]; then
    "$MAMBA" install --yes --prefix "$FFMPEG_PREFIX" --override-channels -c conda-forge "$FFMPEG_PACKAGE"
  else
    "$MAMBA" create --yes --prefix "$FFMPEG_PREFIX" --override-channels -c conda-forge "$FFMPEG_PACKAGE"
  fi
  verify_ffmpeg "$FFMPEG_PREFIX"
}

# Check that every FFmpeg shared library at the required ABI major exists in the prefix.
verify_ffmpeg() {
  prefix="$1"
  for entry in $FFMPEG_ABI; do
    name="${entry%%:*}"
    major="${entry##*:}"
    case "$os" in
      Darwin) library="$prefix/lib/lib$name.$major.dylib" ;;
      *) library="$prefix/lib/lib$name.so.$major" ;;
    esac
    [ -e "$library" ] || { echo "FFmpeg 9 runtime missing: $library" >&2; exit 1; }
  done
  log "Verified FFmpeg 9 libraries ($FFMPEG_ABI) in $prefix/lib"
}

install_server() {
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
    curl -fsSL --retry 3 "https://api.github.com/repos/$REPO/releases/tags/$tag" -o "$tmp/release.json"
  fi
  asset="remedia-server-$target.tar.gz"
  runtime_asset="remedia-server-runtime-$target"
  url="https://github.com/$REPO/releases/download/$tag/$asset"
  log "Selected Remedia $tag for $target."
  installed=""
  installed_binary="$INSTALL_DIR/libexec/remedia-server"
  if [ -f "$INSTALL_DIR/libexec/current.txt" ]; then
    installed_binary="$INSTALL_DIR/libexec/$(cat "$INSTALL_DIR/libexec/current.txt")/remedia-server"
  fi
  if [ -x "$installed_binary" ]; then
    installed="$("$installed_binary" --version 2>/dev/null | head -n 1 | awk '{print $2}')"
  fi
  if [ "$installed" = "${tag#v}" ] && [ -x "$(dirname "$installed_binary")/remedia-server-runtime" ]; then
    log "Remedia $tag is already current; nothing to download or replace."
    exit 0
  fi
  log "Downloading server archive..."
  download_file "$url" "$tmp/server.tar.gz"
  log "Verifying server archive SHA-256..."
  # GitHub places a nested uploader object between an asset's name and digest.
  # Parse the matching asset's fields instead of assuming a fixed line offset.
  curl -fsSL --retry 3 "https://api.github.com/repos/$REPO/releases/tags/$tag" -o "$tmp/release.json"
  digest="$(awk -v wanted="$asset" '
    /"name"[[:space:]]*:/ {
      name = $0
      sub(/^.*"name"[[:space:]]*:[[:space:]]*"/, "", name)
      sub(/".*$/, "", name)
      if (matched) exit
      if (name == wanted) matched = 1
    }
    matched && /"digest"[[:space:]]*:/ {
      checksum = $0
      sub(/^.*"digest"[[:space:]]*:[[:space:]]*"sha256:/, "", checksum)
      sub(/".*$/, "", checksum)
      print checksum
      exit
    }
  ' "$tmp/release.json")"
  case "$digest" in
    *[!a-fA-F0-9]*|'') echo "Release is missing a SHA-256 digest for $asset" >&2; exit 1 ;;
  esac
  [ "${#digest}" -eq 64 ] || { echo "Release has an invalid SHA-256 digest for $asset" >&2; exit 1; }
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
  # Stage both binaries and all licenses before publishing the new installation.
  version_name="$tag-$(basename "$tmp")"
  version_dir="$INSTALL_DIR/libexec/$version_name"
  mkdir -p "$version_dir"
  install -m 755 "$tmp/remedia-server-$target" "$version_dir/remedia-server"
  install -m 755 "$tmp/$runtime_asset" "$version_dir/remedia-server-runtime"
  for item in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt; do
    cp "$tmp/$item" "$INSTALL_DIR/licenses/$item"
  done
}

write_server_commands() {
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
if [ -f "$root/libexec/current.txt" ]; then
  current="$(cat "$root/libexec/current.txt")"
  exec "$root/libexec/$current/remedia-server" "$@"
fi
exec "$root/libexec/remedia-server" "$@"
EOF
  chmod +x "$INSTALL_DIR/bin/remedia"
  cp "$INSTALL_DIR/bin/remedia" "$INSTALL_DIR/bin/remedia-server"
}

ffmpeg_only=0
no_ffmpeg=0
for arg in "$@"; do
  case "$arg" in
    --no-ffmpeg) no_ffmpeg=1 ;;
    --ffmpeg-only) ffmpeg_only=1 ;;
    --help) usage; exit 0 ;;
    *) echo "Unsupported argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done
if [ "$ffmpeg_only" -eq 1 ] && [ "$no_ffmpeg" -eq 1 ]; then
  echo "Conflicting options: --ffmpeg-only installs only FFmpeg and --no-ffmpeg installs only the server, so nothing would be left to install. Remove one of them." >&2
  exit 2
fi
if [ "$ffmpeg_only" -eq 1 ]; then WITH_SERVER=0; fi
if [ "$no_ffmpeg" -eq 1 ]; then WITH_FFMPEG=0; fi

command -v tar >/dev/null || { echo "tar is required" >&2; exit 1; }
# curl is needed for the release lookup and the micromamba download. An existing micromamba makes
# an FFmpeg-only run independent of the network tools and GitHub.
if [ "$WITH_SERVER" -eq 1 ] || { ! command -v micromamba >/dev/null 2>&1 && [ ! -x "$INSTALL_DIR/bin/micromamba" ]; }; then
  command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
fi
os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Darwin:arm64) target=aarch64-apple-darwin ;;
  *)
    if [ "$WITH_SERVER" -eq 1 ]; then
      echo "Unsupported remedia-server platform: $os/$arch" >&2
    else
      echo "Unsupported FFmpeg installer platform: $os/$arch" >&2
    fi
    exit 1
    ;;
esac
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

if [ "$WITH_SERVER" -eq 0 ]; then
  provision_ffmpeg tmp
  log "FFmpeg 9 is installed in $FFMPEG_PREFIX"
  log "@remedia/node finds this installation automatically. Set REMEDIA_FFMPEG_DIR to use another prefix."
  exit 0
fi

install_server

if [ "$WITH_FFMPEG" -eq 1 ]; then
  provision_ffmpeg bin
else
  log "Skipping FFmpeg installation (--no-ffmpeg)."
fi

write_server_commands
configure_shell_path
# Atomic pointer replacement: failures above leave the previous pair active.
printf '%s' "$version_name" > "$INSTALL_DIR/libexec/.current-$$.new"
mv -f "$INSTALL_DIR/libexec/.current-$$.new" "$INSTALL_DIR/libexec/current.txt"
log "Installed: $INSTALL_DIR/bin/remedia"
log "Open a new terminal to use: remedia --version"
