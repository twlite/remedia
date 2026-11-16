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

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

valid_version() (
  directory="$1"
  [ -x "$directory/remedia-server" ] && [ -x "$directory/remedia-server-runtime" ] || exit 1
  [ "$(cat "$directory/version.txt" 2>/dev/null)" = "$tag" ] || exit 1
  [ "$(cat "$directory/target.txt" 2>/dev/null)" = "$target" ] || exit 1
  [ "$(file_sha256 "$directory/remedia-server")" = "$(cat "$directory/cli.sha256" 2>/dev/null)" ] || exit 1
  [ "$(file_sha256 "$directory/remedia-server-runtime")" = "$(cat "$directory/runtime.sha256" 2>/dev/null)" ] || exit 1
  for item in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt LICENSES.md COPYING.GPLv3; do
    [ -s "$directory/licenses/$item" ] || exit 1
  done
)

process_alive() {
  case "$1" in ''|*[!0-9]*) return 1 ;; esac
  ps -p "$1" -o pid= 2>/dev/null | grep -q '[0-9]'
}

# Unknown process visibility means keep files. No running server is signalled.
directory_unused() (
  command -v lsof >/dev/null 2>&1 || exit 1
  result=0
  lsof -t +D "$1" > "$tmp/open-pids" 2> "$tmp/lsof-errors" || result=$?
  [ "$result" -eq 1 ] && [ ! -s "$tmp/open-pids" ] && [ ! -s "$tmp/lsof-errors" ]
)

cleanup_versions() {
  active="$(cat "$libexec/current.txt")"
  previous="$(cat "$libexec/previous.txt" 2>/dev/null || true)"
  # A migrated duplicate is retained for one invocation and while any old process uses it.
  case "$previous" in
    "$active"-tmp.*)
      if [ "$old_current" != "$previous" ] && directory_unused "$libexec/$previous"; then
        rm -f "$libexec/previous.txt"
        previous=""
      fi ;;
  esac
  for directory in "$libexec"/.stage-* "$libexec"/v*; do
    [ -d "$directory" ] && [ ! -L "$directory" ] || continue
    name="$(basename "$directory")"
    [ "$name" != "$active" ] && [ "$name" != "$previous" ] || continue
    case "$name" in
      .stage-*)
        stem="${name%.*}"
        owner="${stem##*-}"
        if ! process_alive "$owner"; then rm -rf "$directory"; fi
        continue ;;
    esac
    # Only installer-owned version directories and the historical tmp layout.
    if [ ! -f "$directory/version.txt" ]; then
      printf '%s\n' "$name" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?-tmp\.[A-Za-z0-9]+$' || continue
    fi
    if directory_unused "$directory"; then
      rm -rf "$directory"
    else
      log "Retaining $name: in use or process inspection unavailable. A later install can clean it."
    fi
  done
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
  printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$' || { echo "Invalid release tag" >&2; exit 1; }
  version_name="$tag"
  version_dir="$libexec/$version_name"
  if valid_version "$version_dir"; then
    log "Remedia $tag is already current or cached; nothing to download or replace."
    return
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
  # Stage on the destination filesystem, then rename the complete pair into place.
  stage="$(mktemp -d "$libexec/.stage-$tag-$$.XXXXXX")"
  install -m 755 "$tmp/remedia-server-$target" "$stage/remedia-server"
  install -m 755 "$tmp/$runtime_asset" "$stage/remedia-server-runtime"
  mkdir "$stage/licenses"
  for item in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt LICENSES.md COPYING.GPLv3; do
    cp "$tmp/$item" "$stage/licenses/$item"
  done
  printf '%s' "$target" > "$stage/target.txt"
  file_sha256 "$stage/remedia-server" > "$stage/cli.sha256"
  file_sha256 "$stage/remedia-server-runtime" > "$stage/runtime.sha256"
  printf '%s' "$tag" > "$stage/version.txt"
  valid_version "$stage" || { echo "Incomplete staged release" >&2; exit 1; }
  if [ -e "$version_dir" ]; then
    # Never repair a fixed path underneath an active or possibly running process.
    [ "$old_current" != "$tag" ] && [ "$(cat "$libexec/previous.txt" 2>/dev/null || true)" != "$tag" ] && directory_unused "$version_dir" || {
      echo "Existing $tag is incomplete or differs; preserving its files. Select another version or finish processes using it before repair." >&2
      exit 1
    }
    rm -rf "$version_dir"
  fi
  mv "$stage" "$version_dir"
  stage=""

}

write_server_commands() {
  cat > "$tmp/wrapper" <<'EOF'
#!/bin/sh
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
if [ -f "$root/libexec/current.txt" ]; then
  current="$(cat "$root/libexec/current.txt")"
  exec "$root/libexec/$current/remedia-server" "$@"
fi
exec "$root/libexec/remedia-server" "$@"
EOF
  for command in remedia remedia-server; do
    install -m 755 "$tmp/wrapper" "$INSTALL_DIR/bin/.$command-$$.new"
    mv -f "$INSTALL_DIR/bin/.$command-$$.new" "$INSTALL_DIR/bin/$command"
  done
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
libexec="$INSTALL_DIR/libexec"
mkdir -p "$libexec" "$INSTALL_DIR/bin" "$INSTALL_DIR/licenses"
tmp="$(mktemp -d "$libexec/.stage-download-$$.XXXXXX")"
stage=""
cleanup_install() {
  result=$?
  trap - EXIT HUP INT TERM
  [ -z "$stage" ] || rm -rf "$stage"
  rm -f "$libexec/.current-$$.new" "$libexec/.previous-$$.new" "$INSTALL_DIR/bin/.remedia-$$.new" "$INSTALL_DIR/bin/.remedia-server-$$.new"
  rm -rf "$tmp"
  exit "$result"
}
trap cleanup_install EXIT
trap 'exit 1' HUP INT TERM
# Descriptor locks are released by the OS even after uncatchable termination.
# Keep the lock inode in place; unlinking a held lock could admit another installer.
exec 9> "$libexec/.install.lock"
if command -v flock >/dev/null 2>&1; then
  flock -n 9 || { echo "Another Remedia installer is running" >&2; exit 1; }
elif command -v lockf >/dev/null 2>&1; then
  lockf -s -t 0 9 || { echo "Another Remedia installer is running" >&2; exit 1; }
else
  echo "flock (Linux/BusyBox) or lockf (macOS) is required for safe installation" >&2
  exit 1
fi
old_current="$(cat "$libexec/current.txt" 2>/dev/null || true)"
case "$old_current" in *[!A-Za-z0-9._+-]*|.*) echo "Invalid current installation pointer" >&2; exit 1 ;; esac
install_server
write_server_commands
configure_shell_path
# Notices are also retained in each version so cached rollback preserves them.
for item in LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt LICENSES.md COPYING.GPLv3; do
  cp "$version_dir/licenses/$item" "$INSTALL_DIR/licenses/$item"
done
previous="$(cat "$libexec/previous.txt" 2>/dev/null || true)"
if [ -n "$old_current" ] && [ "$old_current" != "$version_name" ]; then
  printf '%s' "$old_current" > "$libexec/.previous-$$.new"
  mv -f "$libexec/.previous-$$.new" "$libexec/previous.txt"
fi
printf '%s' "$version_name" > "$libexec/.current-$$.new"
if ! mv -f "$libexec/.current-$$.new" "$libexec/current.txt"; then
  if [ -n "$previous" ]; then
    printf '%s' "$previous" > "$libexec/.previous-$$.new"
    mv -f "$libexec/.previous-$$.new" "$libexec/previous.txt"
  else
    rm -f "$libexec/previous.txt"
  fi
  exit 1
fi
cleanup_versions || log "Retaining obsolete directories after cleanup failure."
log "Installed: $INSTALL_DIR/bin/remedia ($version_name)"
log "Open a new terminal to use: remedia --version"
