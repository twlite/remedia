# Obtain and rebuild corresponding source

Original Remedia source is MIT. Official FFmpeg-linked binaries are distributed
under GPLv3-or-later; see LICENSES.md and COPYING.GPLv3.

Use the binary's Remedia version (`remedia --version`, the npm version, or
`getFFmpegRuntime().remediaVersion`; the function was `getFfmpegRuntime()` in v0.3.0).
Open https://github.com/twlite/remedia/releases/tag/vVERSION.

For v0.3.0, download the unchanged `corresponding-source.tar.gz` and follow its
BUILD.txt. That release has no source-manifest.json. Releases after v0.3.0 use the
implemented mechanism below: download `source-manifest.json` and
`corresponding-source.tar.gz` from the matching binary release.

The manifest names the exact Remedia commit, the SHA-256 and size of the release
source archive, all target records, and a public content-addressed dependency
snapshot. Download `dependencies.tar.gz` from the manifest's dependencies.url.
Check both archives against their size and SHA-256 in the manifest. The releaseSource.url
is a public copy of the release source archive, available before npm publication;
the binary release also carries a copy. This is actual
retained source, not an instruction to retrieve moving upstream branches. No login,
fee, or binary download is required. Source assets have immutable names and are never overwritten. New release-specific
archives are added to the source-only release for the matching dependency identity.
Snapshot identity format v2 hashes Cargo.lock, Cargo manifests/configuration, the
pinned Rust toolchain, native input/patch metadata, and the vendoring, filtering,
patching and archive recipes. Changing these inputs selects a new source snapshot;
archive bytes are verified separately by SHA-256. Keep the format version in sync
with any output-affecting tool semantics not represented by those inputs.
A retry reuses verified public dependencies and matching release-source assets;
partial uploads are retried without overwriting completed mismatched assets.
Source snapshots must remain available for as
long as any associated binaries are distributed. Redistributors must retain them
or provide equivalent source access, with these directions next to their binaries.

Extract the release archive into `release-source/` and `remedia.tar` into a working
directory. Apply `release.patch` with `git apply --binary` in that directory. It
records version synchronization and other generated tracked release changes.
Extract the dependency snapshot separately, then copy its `vendor/` and `native/`
into the working directory. Merge `vendor-config.toml` into `.cargo/config.toml`
(the directory is relative to the repository root). Set:

```sh
export REMEDIA_SOURCE_DIR="$PWD/native"
export CARGO_NET_OFFLINE=true
export REMEDIA_BUILD_VERSION="<manifest version>"
export REMEDIA_BUILD_COMMIT="<manifest commit>"
export GITHUB_WORKSPACE="$PWD"
export RUNNER_TEMP="<absolute build directory>"
export GITHUB_ENV="$RUNNER_TEMP/environment"
mkdir -p "$RUNNER_TEMP"
```

Use the pinned rust-toolchain.toml. Follow .github/workflows/platform.yaml for the
chosen target's host tools, compiler, sysroot, SDK and environment. These are
unmodified generally available build tools / platform components; the snapshot is
not a toolchain installer. The exact generated FFmpeg configuration, compiler
identity and Rust compiler are in targets/native-source-TARGET.tar.gz. Paths in
these records describe the original runner and must be adjusted locally.

POSIX native: run `bash .github/scripts/build-ffmpeg-static.sh`. Cross builds:
set RUST_TARGET, install the workflow's pinned Zig/NDK or cross compiler and run
`bash .github/scripts/build-ffmpeg-cross.sh`. Read the environment file into the
build shell (GitHub Actions does this between steps). Windows: use PowerShell,
the pinned MSVC environment, and `.github/scripts/build-ffmpeg-windows.ps1`.
The retained vcpkg tree includes all port patches and build scripts; the Remedia
script applies the audio feature configuration. One FFmpeg source archive is shared across targets, retained in the original
checksum-verified encoding required by the Windows port. It has the same source
file contents as the POSIX Git revision; offline POSIX extraction restores its
short-commit VERSION metadata.
No vcpkg installed libraries, buildtrees, or download caches are source inputs.
`omitted-prebuilt-files.txt` records vendored test binaries and unsupported-target
objects/import stubs removed from the snapshot; their source remains available.
The Rust Windows crates' required MSVC System Library import stubs are retained.
General-purpose compilers, SDKs and OS libraries are not copied into the snapshot.

Before compiling vendored Rust sources, regenerate the NAPI-RS helper with
`node .github/scripts/prepare-vendor.mjs "$PWD/vendor"`. This uses a host Clang with
a WebAssembly backend (the workflow's LLVM/Clang); on macOS use Homebrew LLVM's
Clang via `REMEDIA_WASI_CC` if Apple's Clang lacks that backend. The upstream C
source and flags are retained; the helper is not linked into released native targets.

For 32-bit targets run .github/scripts/patch-ffmpeg-sys-vulkan.mjs before compiling;
it also supports the vendored crate. Build the server with cargo build --release
--locked -p remedia-server --bins and the addon with the workflow's NAPI-RS command.
The pnpm lockfile and original TypeScript sources are included. NAPI-RS's generated
loader/declarations are regenerated from Rust; distributed JS is built by the
package build:types command. Platform runtime tests and the clean packed-package
smoke test are in the same workflow. No FFmpeg executable is used for playback.

CI checks pinned source hashes, vendored dependency completeness, target records,
license policy, and public snapshot status before native npm publication. When
source inputs change, validate a fresh snapshot and an offline build using it;
repeat target rebuild validation when toolchains or recipes change. These checks
support source distribution but do not establish complete legal compliance.
Installation information may additionally be required for GPLv3 User Products;
redistributors must assess their own distribution and platform restrictions.
