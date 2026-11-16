#!/bin/sh
set -eu

REPO="twlite/remedia"
INSTALL_DIR="${REMEDIA_INSTALL_DIR:-$HOME/.remedia}"
VERSION="${REMEDIA_VERSION:-latest}"
log() {
  printf '[remedia] %s\n' "$*"
}

usage() {
  cat <<'EOF'
Usage: install.sh [--no-ffmpeg]

Install the self-contained Remedia server. --no-ffmpeg is accepted as a no-op.

Environment:
  REMEDIA_INSTALL_DIR   Installation prefix (default: ~/.remedia)
  REMEDIA_VERSION       Server release, latest or x.y.z
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
    case "$item" in "remedia-server-$target"|"remedia-server-runtime-$target"|LICENSE|THIRD_PARTY_NOTICES.md|THIRD_PARTY_LICENSES.txt|LICENSES.md|COPYING.GPLv3) ;; *) echo "Unexpected archive entry $item" >&2; exit 1 ;; esac
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
  for item in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt LICENSES.md COPYING.GPLv3; do
    cp "$tmp/$item" "$INSTALL_DIR/licenses/$item"
  done
}

write_server_commands() {
  cat > "$INSTALL_DIR/bin/remedia" <<'EOF'
#!/bin/sh
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
if [ -f "$root/libexec/current.txt" ]; then
  current="$(cat "$root/libexec/current.txt")"
  exec "$root/libexec/$current/remedia-server" "$@"
fi
exec "$root/libexec/remedia-server" "$@"
EOF
  chmod +x "$INSTALL_DIR/bin/remedia"
  cp "$INSTALL_DIR/bin/remedia" "$INSTALL_DIR/bin/remedia-server"
}

for arg in "$@"; do
  case "$arg" in
    --no-ffmpeg) ;;
    --help) usage; exit 0 ;;
    *) echo "Unsupported argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done
command -v tar >/dev/null || { echo "tar is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Darwin:arm64) target=aarch64-apple-darwin ;;
  *)
    echo "Unsupported remedia-server platform: $os/$arch" >&2
    exit 1
    ;;
esac
if [ "$os" = Linux ] && { ldd --version 2>&1 | grep -qi musl || ls /lib/ld-musl-*.so.1 >/dev/null 2>&1; }; then
  target="${target%-gnu}-musl"
fi
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

install_server

write_server_commands
configure_shell_path
# Atomic pointer replacement: failures above leave the previous pair active.
printf '%s' "$version_name" > "$INSTALL_DIR/libexec/.current-$$.new"
mv -f "$INSTALL_DIR/libexec/.current-$$.new" "$INSTALL_DIR/libexec/current.txt"
log "Installed: $INSTALL_DIR/bin/remedia"
log "Open a new terminal to use: remedia --version"
