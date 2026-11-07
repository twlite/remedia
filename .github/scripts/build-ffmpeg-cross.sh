#!/usr/bin/env bash
set -euo pipefail

target="${RUST_TARGET:?RUST_TARGET must name the Rust compilation target}"
prefix="${FFMPEG_DIR:-${RUNNER_TEMP:?RUNNER_TEMP must be set}/remedia-ffmpeg-${target}}"
source_dir="${RUNNER_TEMP:?RUNNER_TEMP must be set}/remedia-ffmpeg-source-${target}"
tools_dir="${RUNNER_TEMP:?RUNNER_TEMP must be set}/remedia-target-tools-${target}"
ffmpeg_commit='946fcce07b6dcd0331c8cc609192aeff5e1924f8'
marker="$prefix/.remedia-ffmpeg-build"

if [[ -f "$marker" ]] && [[ "$(cat "$marker")" == "$ffmpeg_commit:$target" ]]; then
  echo "Using cached FFmpeg 9.0.2 target libraries at $prefix"
else
  rm -rf "$prefix" "$source_dir"
  mkdir -p "$prefix" "$tools_dir"

  git init -q "$source_dir"
  git -C "$source_dir" remote add origin https://github.com/FFmpeg/FFmpeg.git
  git -C "$source_dir" fetch --quiet --depth=1 origin "$ffmpeg_commit"
  git -C "$source_dir" checkout --quiet --detach FETCH_HEAD

  local_cc=''
  local_ar=''
  local_nm=''
  local_ranlib=''
  local_strip=''
  local_as=''
  target_os='linux'
  target_arch=''
  target_cpu=''
  target_sysroot=''
  zig_triple=''

  case "$target" in
    x86_64-unknown-linux-musl)
      target_arch='x86_64'
      zig_triple='x86_64-linux-musl'
      ;;
    aarch64-unknown-linux-musl)
      target_arch='aarch64'
      zig_triple='aarch64-linux-musl'
      ;;
    armv7-unknown-linux-gnueabihf)
      target_arch='arm'
      target_cpu='armv7-a'
      local_cc='arm-linux-gnueabihf-gcc'
      local_as="$local_cc"
      local_ar='arm-linux-gnueabihf-ar'
      local_nm='arm-linux-gnueabihf-nm'
      local_ranlib='arm-linux-gnueabihf-ranlib'
      local_strip='arm-linux-gnueabihf-strip'
      # Let the cross-GCC driver use its own tested sysroot and multilib search paths.
      ;;
    aarch64-linux-android)
      target_os='android'
      target_arch='aarch64'
      target_sysroot="${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must point to the installed NDK}/toolchains/llvm/prebuilt/linux-x86_64/sysroot"
      local_cc="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang"
      local_as="$local_cc"
      local_ar="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"
      local_nm="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-nm"
      local_ranlib="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ranlib"
      local_strip="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip"
      ;;
    armv7-linux-androideabi)
      target_os='android'
      target_arch='arm'
      target_cpu='armv7-a'
      target_sysroot="${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must point to the installed NDK}/toolchains/llvm/prebuilt/linux-x86_64/sysroot"
      local_cc="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi24-clang"
      local_as="$local_cc"
      local_ar="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"
      local_nm="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-nm"
      local_ranlib="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ranlib"
      local_strip="${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip"
      ;;
    *)
      echo "No target FFmpeg recipe for $target" >&2
      exit 2
      ;;
  esac

  if [[ -n "$zig_triple" ]]; then
    local_cc="$tools_dir/cc"
    local_as="$tools_dir/as"
    cat > "$local_cc" <<EOF
#!/usr/bin/env bash
set -euo pipefail
# cc-rs adds the Rust triple as --target=...; Zig requires the normalized
# target below and cannot parse the Rust 'unknown' vendor in that flag.
args=()
for arg in "\$@"; do
  case "\$arg" in
    --target=*) ;;
    # The Zig 0.14 bundled linker does not accept GNU ld's Cortex-A53
    # erratum flag. Keep this target experimental until runtime verified.
    -Wl,--fix-cortex-a53-843419) ;;
    *) args+=("\$arg") ;;
  esac
done
exec zig cc -target '$zig_triple' "\${args[@]}"
EOF
    cat > "$local_as" <<EOF
#!/usr/bin/env bash
exec zig cc -target '$zig_triple' "\$@"
EOF
    cat > "$tools_dir/ar" <<'EOF'
#!/usr/bin/env bash
exec zig ar "$@"
EOF
    cat > "$tools_dir/nm" <<'EOF'
#!/usr/bin/env bash
exec llvm-nm "$@"
EOF
    cat > "$tools_dir/ranlib" <<'EOF'
#!/usr/bin/env bash
exec zig ranlib "$@"
EOF
    cat > "$tools_dir/strip" <<'EOF'
#!/usr/bin/env bash
exec llvm-strip "$@"
EOF
    chmod +x "$local_cc" "$local_as" "$tools_dir/ar" "$tools_dir/nm" "$tools_dir/ranlib" "$tools_dir/strip"
    local_ar="$tools_dir/ar"
    local_nm="$tools_dir/nm"
    local_ranlib="$tools_dir/ranlib"
    local_strip="$tools_dir/strip"
  fi

  for tool in "$local_cc" "$local_ar" "$local_nm" "$local_ranlib" "$local_strip"; do
    command -v "$tool" >/dev/null 2>&1 || {
      echo "Required target tool is unavailable: $tool" >&2
      exit 2
    }
  done

  target_env="${target//-/_}"
  target_env_upper="$(printf '%s' "$target_env" | tr '[:lower:]' '[:upper:]')"
  linker="$local_cc"
  for name in "$target" "$target_env" "$target_env_upper"; do
    printf 'CC_%s=%s\n' "$name" "$local_cc" >> "$GITHUB_ENV"
    printf 'AR_%s=%s\n' "$name" "$local_ar" >> "$GITHUB_ENV"
  done
  printf 'CARGO_TARGET_%s_LINKER=%s\n' "$target_env_upper" "$linker" >> "$GITHUB_ENV"

  configure_args=(
    "--prefix=$prefix"
    "--libdir=$prefix/lib"
    "--shlibdir=$prefix/lib"
    "--target-os=$target_os"
    "--arch=$target_arch"
    --enable-cross-compile
    "--cc=$local_cc"
    "--as=$local_as"
    "--ar=$local_ar"
    "--nm=$local_nm"
    "--ranlib=$local_ranlib"
    "--strip=$local_strip"
    --host-cc=cc
    --enable-shared
    --disable-static
    --disable-programs
    --disable-doc
    --disable-debug
    --disable-autodetect
    --enable-pic
    --enable-network
  )
  if [[ -n "$target_cpu" ]]; then
    configure_args+=("--cpu=$target_cpu")
  fi
  if [[ "$target" == 'armv7-unknown-linux-gnueabihf' ]]; then
    # Ubuntu's ARM hard-float cross GCC needs an explicit FPU when FFmpeg
    # sets -march=armv7-a (otherwise cc1 reports no FPU for -mfloat-abi=hard).
    configure_args+=("--extra-cflags=-mfpu=neon -mfloat-abi=hard")
  fi
  if [[ -n "$target_sysroot" ]]; then
    configure_args+=("--sysroot=$target_sysroot")
  fi

  pushd "$source_dir" >/dev/null
  if ! ./configure "${configure_args[@]}"; then
    cat ffbuild/config.log >&2 || true
    exit 1
  fi
  make -j"${FFMPEG_BUILD_JOBS:-2}"
  make install
  popd >/dev/null

  printf '%s:%s\n' "$ffmpeg_commit" "$target" > "$marker"
fi

export FFMPEG_DIR="$prefix"
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
export PKG_CONFIG_ALLOW_CROSS=1

declare -A expected_majors=(
  [libavcodec]=63
  [libavformat]=63
  [libavutil]=61
  [libavfilter]=12
  [libswresample]=7
)
for library in "${!expected_majors[@]}"; do
  version="$(pkg-config --modversion "$library")"
  major="${version%%.*}"
  echo "$library $version from $PKG_CONFIG_LIBDIR"
  if [[ "$major" != "${expected_majors[$library]}" ]]; then
    echo "$library must use FFmpeg 9 ABI ${expected_majors[$library]}, found $version" >&2
    exit 1
  fi
done

if [[ -n "${GITHUB_ENV:-}" ]]; then
  {
    echo "FFMPEG_DIR=$prefix"
    echo "PKG_CONFIG_PATH=$prefix/lib/pkgconfig"
    echo "PKG_CONFIG_LIBDIR=$prefix/lib/pkgconfig"
    echo 'PKG_CONFIG_ALLOW_CROSS=1'
  } >> "$GITHUB_ENV"
fi
