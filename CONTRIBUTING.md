# Contributing to Remedia

Remedia is under development. Contributions that improve the existing code or make the project
easier to build and maintain are welcome. Playback, transport, server, and binding features should
follow the intended boundaries in [`docs/architecture.md`](docs/architecture.md).

## Prerequisites

- Rust through rustup. The workspace declares Rust 1.88 as its minimum supported version; this
  repository pins Rust 1.99.1 for development and requests `rustfmt` and `clippy` in
  `rust-toolchain.toml`. Rustup installs the selected toolchain when first needed.
- Node.js 22.18 or newer.
- pnpm 12.16.1, selected by the root `package.json`.
- Git.

`remedia-core` dynamically links FFmpeg libraries through `ffmpeg-next` and `ffmpeg-sys-next`. Its
Opus adapter builds libopus from bundled source and links it statically. Playback filters use
libavfilter. Install development headers for libavformat, libavcodec, libavutil, libavfilter, and
libswresample, plus `pkg-config` and Clang/libclang for the bindings build. Remedia does not require
system libopus headers or libraries.
The CI workflow lists all 13 requested Rust targets. The five primary targets below run native integration tests. The release packer requires addon
artifacts for all 13 targets, including the additional cross-build targets.

| Target                  | CI level     | Native verification                                                                            |
| ----------------------- | ------------ | ---------------------------------------------------------------------------------------------- |
| Linux x86_64 GNU        | Required     | Native FFmpeg 7 build, Rust tests, N-API and server runtime tests on Node.js 22.18, 24, and 26 |
| Linux ARM64 GNU         | Required     | Native FFmpeg 7 build, Rust tests, N-API and server runtime tests on Node.js 22.18, 24, and 26 |
| macOS x86_64            | Required     | Native FFmpeg 7 build, Rust tests, N-API and server runtime tests on Node.js 22.18, 24, and 26 |
| macOS ARM64             | Required     | Native FFmpeg 7 build, Rust tests, N-API and server runtime tests on Node.js 22.18, 24, and 26 |
| Windows x86_64 MSVC     | Required     | Native FFmpeg 7 build, Rust tests, N-API and server runtime tests on Node.js 22.18, 24, and 26 |
| Linux x86_64 musl       | Experimental | Build-only; pinned FFmpeg 7.1.1 cross-build with Zig; runtime compatibility is unverified      |
| Linux ARM64 musl        | Experimental | Build-only; pinned FFmpeg 7.1.1 cross-build with Zig; runtime compatibility is unverified      |
| Linux ARMv7 GNU         | Experimental | Build-only; pinned FFmpeg 7.1.1 uses the hard-float GNU cross toolchain and sysroot            |
| Windows x86 MSVC        | Experimental | Build-only; target-specific FFmpeg 7.1.1 x86 libraries use pinned vcpkg                        |
| Windows ARM64 MSVC      | Experimental | Build-only cross-compile; target-specific FFmpeg 7.1.1 ARM64 libraries use pinned vcpkg        |
| Android ARM64 and ARMv7 | Experimental | Build-only; pinned NDK 27.2, API 24 linkers, and target-built FFmpeg 7.1.1                     |
| FreeBSD x86_64          | Experimental | FreeBSD 15.1 VM builds FFmpeg 7 and runs native build and runtime tests                        |

All 13 N-API targets are included in npm release packaging. Standalone server binaries cover only
the five primary targets. WASI is not in the workflow. Build-only coverage does not establish native
runtime compatibility.

Remedia targets the FFmpeg 7 ABI and dynamically links FFmpeg. Its Opus adapter builds libopus from
the locked bundled source and statically links it into native binaries. FFmpeg executables are used
only for dependency/version checks; Remedia does not spawn them for playback. The CI dependency
specifications are in `.github/native-deps/` and pin FFmpeg 7.1.1 and pkg-config 0.29.2 for Linux
and Windows. The Linux and Android cross-build recipe is in
`.github/scripts/build-ffmpeg-cross.sh`; Windows x86 and ARM64 use the pinned target triplets in
`.github/native-deps/windows-cross/vcpkg.json`. Experimental targets are not runtime-verified except
for the FreeBSD VM job, and their artifacts are published as additional platform packages. A successful
cross-build alone does not establish that the target runtime can load the addon or its FFmpeg shared
libraries.

### macOS

Install Xcode Command Line Tools, versioned FFmpeg 7, and LLVM 19.1.7 for the
repository's bindgen version:

```sh
brew install ffmpeg@7 pkg-config llvm@19
export FFMPEG_DIR="$(brew --prefix ffmpeg@7)"
export PKG_CONFIG_PATH="$FFMPEG_DIR/lib/pkgconfig"
export LIBCLANG_PATH="$(brew --prefix llvm@19)/lib"
export DYLD_LIBRARY_PATH="$FFMPEG_DIR/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
```

The versioned formula keeps the FFmpeg major version explicit. After a Homebrew upgrade, rerun the
`brew --prefix` assignments in the current shell; do not retain a path into a versioned Cellar
directory. Keep the versioned LLVM 19 formula installed and confirm that its prefix contains
`lib/libclang.dylib` if bindgen reports missing or incompatible FFmpeg layouts.

### Linux

Install Clang/libclang, then create the pinned dynamic FFmpeg environment used by CI:

```sh
sudo apt-get install clang libclang-dev
micromamba create --name remedia-ffmpeg7 --file .github/native-deps/ffmpeg7.yml
micromamba activate remedia-ffmpeg7
export FFMPEG_DIR="$CONDA_PREFIX"
export PKG_CONFIG_PATH="$CONDA_PREFIX/lib/pkgconfig"
export LD_LIBRARY_PATH="$CONDA_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
```

The FFmpeg packages in the Ubuntu runner's default repositories may have a different major version;
use the pinned environment rather than mixing system FFmpeg headers with another runtime library.

### Windows

Install Visual Studio 2022 Build Tools with the MSVC C++ workload, Rust's MSVC toolchain, and
micromamba. Use the pinned x64 development packages:

```powershell
micromamba create --name remedia-ffmpeg7 --file .github/native-deps/ffmpeg7-windows.yml
micromamba activate remedia-ffmpeg7
$env:FFMPEG_DIR = Join-Path $env:CONDA_PREFIX 'Library'
$env:PKG_CONFIG_PATH = Join-Path $env:FFMPEG_DIR 'lib\pkgconfig'
$env:LIBCLANG_PATH = Join-Path $env:CONDA_PREFIX 'Library\bin'
$env:PATH = (Join-Path $env:FFMPEG_DIR 'bin') + ';' + $env:PATH
```

The supported x64 FFmpeg package must include MSVC import libraries, headers, and matching x64 DLLs.
Keep the runtime DLL directory on `PATH` when running tests and applications. Experimental x86 and
ARM64 builds use isolated, target-specific vcpkg triplets; never reuse the x64 libraries for those
targets. MinGW libraries are not compatible with the MSVC target. Windows builds use MSVC's dynamic
CRT to match the bundled Opus CMake configuration. If the runtime machine does not already have a
compatible Microsoft Visual C++ v14 runtime, install the current Visual C++ Redistributable.
Remedia does not bundle that runtime, and the N-API package does not promise static CRT linkage.

## Set up the repository

```sh
git clone https://github.com/twlite/remedia.git
cd remedia
corepack enable
pnpm --version
pnpm install --frozen-lockfile
```

The pnpm version should be `12.16.1`. If Corepack is unavailable, install that version with
`npm install --global pnpm@12.16.1`.

Rustup reads `rust-toolchain.toml` automatically. To install the pinned toolchain and its requested
components explicitly, run:

```sh
rustup toolchain install 1.99.1 --component rustfmt --component clippy
```

## Development commands

Run these from the repository root:

```sh
pnpm lint             # lint JavaScript and TypeScript with Oxlint
pnpm lint:fix         # apply Oxlint's safe automatic fixes
pnpm format           # format supported files with Oxfmt
pnpm format:check     # check formatting without changing files
pnpm typecheck        # run each workspace package's typecheck command
pnpm check            # run the full local validation sequence
pnpm --filter discord-app test
pnpm --filter discord-app start  # manual Discord integration; requires apps/discord/.env
pnpm --filter @remedia/client test
pnpm --filter @remedia/client build

cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p remedia-server --all-targets
pnpm --filter docs build
```

The docs app can be run with `pnpm --filter docs dev` and built with
`pnpm --filter docs build`. `pnpm --filter @remedia/node test` builds the debug N-API addon and runs
the TypeScript integration tests with Vitest. To separate compilation from execution, run
`pnpm --filter @remedia/node build:native` (release addon),
`pnpm --filter @remedia/node build:types`, then
`pnpm --filter @remedia/node test:bindings`. `test:bindings` never compiles native code and is the
CI entry point after downloading a target artifact. Add tests under
`packages/remedia-node/tests/` and split them by behavior; `typecheck` checks both library and test
TypeScript. The package keeps Rust code in `src/`, TypeScript library code in `lib/`, and generated
TypeScript output in `dist/`.
The Discord harness runs TypeScript directly with `tsx`; its `typecheck` script uses `tsc --noEmit`,
and its local queue tests run with Node's built-in test runner. To start the bot, run
`pnpm --filter discord-app start`. This builds the native Node package first and reads
`DISCORD_TOKEN` and `APPLICATION_ID` from `apps/discord/.env`.
`@remedia/client` is pure TypeScript ESM. Remote-only consumers do not need the native addon;
`@remedia/node` is an optional peer dependency for embedded nodes. Its default `test` command excludes
the native hybrid and Rust-server integration files. Run `pnpm --filter @remedia/client test:hybrid`
to require and test a built native addon. Run `pnpm --filter @remedia/client test:server` to test
against a real local server; it uses `REMEDIA_SERVER_BINARY` when provided and otherwise runs
`cargo run` for convenience. CI always invokes both explicit integration commands, with a previously
built addon and server executable, so a missing environment variable cannot silently skip either
test.

Compiling `@remedia/node` from source requires the pinned Rust toolchain, target-compatible FFmpeg 7
development libraries, and libclang. The bundled Opus source is built and statically linked by
Cargo. Installing a prebuilt platform addon from the
published package does not need those build tools and does not install FFmpeg during package
installation. The published N-API package does not bundle FFmpeg or compile native code during
npm/pnpm installation. Consumers must install FFmpeg 7 shared runtime libraries for a supported
target; see the [Node.js distribution guide](apps/docs/content/docs/node/distribution.mdx).

## CI change selection

Push and pull-request CI runs for workspace code, application documentation content, build and
release configuration, and distributed license files. README-only changes, contributor guides,
and issue/PR templates do not start the workflow. Manual workflow dispatch runs all checks.

Changes confined to `apps/docs`, `apps/discord`, shared npm lockfiles, or license texts run the
JavaScript, packaging-test, and documentation checks without installing FFmpeg or running Rust
checks, native build matrices, or native release packing. Rust crates, the Node and client packages
(which exercise native integration), native build recipes, workflow scripts, and shared build
configuration enable the native checks. Release commits that start the workflow also enable them.
Push comparisons cover the entire push; pull requests compare from the merge base. New feature
branches compare with the default branch. Unavailable history falls back to full validation.
Run `pnpm test:ci` to check this selection logic locally.

## Automated releases

Stable and prerelease releases are created by pushing a commit to main whose entire subject is
'chore: release x.y.z' or 'chore: release x.y.z-tag.n'. Stable versions publish with npm's
'latest' dist-tag. Prerelease versions use their lowercase identifier as the dist-tag, such as
'canary' in '1.3.0-canary.5'. Ordinary pushes and pull requests never publish. A release commit
must include a path accepted by the workflow filter, such as a package manifest change; an empty
or README-only release marker does not start CI.

The workflow overrides the @remedia/node and @remedia/client npm versions in each CI checkout
before installing, building, or packing. It does not commit those overrides or alter Rust crate
versions. @napi-rs/cli creates platform packages and synchronizes their versions; pnpm pack
resolves the client's workspace:* peer dependency in the packed manifest. All 13 configured
addon targets are packed and published. Exact package versions already on npm are skipped, and
platform packages publish before @remedia/node, followed by @remedia/client.

For token-free releases, configure npm Trusted Publishing for each package the workflow publishes:

- @remedia/node-linux-x64-gnu
- @remedia/node-linux-arm64-gnu
- @remedia/node-darwin-x64
- @remedia/node-darwin-arm64
- @remedia/node-win32-x64-msvc
- @remedia/node
- @remedia/client

For every package, choose GitHub Actions with organization/user twlite, repository remedia, and
workflow filename ci.yaml. Leave the environment unset and allow direct publishing. Alternatively,
set the optional repository Actions secret `NPM_TOKEN` to a token with publish access to the release
packages. The publish step configures token credentials only when the secret is present; otherwise
it relies on OIDC Trusted Publishing. npm 11.21.0 can still prefer OIDC when Trusted Publishing is
already configured; the optional token also supports publishing when OIDC is unavailable.
The workflow uses a GitHub-hosted Node 24 runner and npm 11.21.0; only its publish job receives
id-token: write, also used for provenance when publishing with a token. See npm's
[trusted publisher setup guide](https://docs.npmjs.com/trusted-publishers/).
`cargo test -p remedia-core` runs deterministic file, in-memory, streaming, and local-HTTP decoder tests. The
core tests cover decoder conversion, seeking, playback lifecycle, native FFmpeg filtergraph processing,
master gain, Opus framing, backpressure, and stop cleanup. `remedia-discord` tests parsing, fail-closed DAVE behavior, RTP, transport encryption,
UDP discovery, pacing, and bounded queue cancellation without Discord credentials. The
examples can be run with `cargo run -p remedia-core --example decode -- file <path>`,
`cargo run -p remedia-core --example decode -- url <http(s)-url>`, or
`cargo run -p remedia-core --example playback -- file <path>`.
The optional manual voice sender example is `cargo run -p remedia-discord --example play_file`; it
requires externally obtained Discord voice session information and a reachable voice endpoint.
The standalone server reads `remedia.config.json` from the current directory or a `--config PATH`
override. Use `crates/remedia-server/remedia.config.example.json` as the schema example. Its
`remote_discord` example requires an already running server and fresh voice credentials obtained by
an external Discord Gateway client; normal tests do not require Discord credentials.

## Where code belongs

- Add a Rust crate under `crates/` when it has an independently useful boundary in the engine,
  transport, or server. `remedia-core` owns source opening, decoding, resampling, playback sessions,
  generic PCM output, and transport-independent Opus encoding. `remedia-discord` owns Discord voice
  connections, DAVE, RTP, encryption, UDP, and packet scheduling. `remedia-server` owns authenticated
  remote sessions and JSON RPC while reusing those crates. Add crates to the Cargo workspace only
  when they exist and are needed.
- Add a JavaScript or TypeScript package under `packages/` when it has a distinct package boundary.
  Keep an N-API crate alongside its owning Node package: `packages/remedia-node` is included in the
  root Cargo workspace and calls the Remedia engine in process. Keep its Rust files in `src/` and
  TypeScript files in `lib/`; do not mix languages in `src/`.
- Keep documentation tooling and site code under `apps/docs/`.
- Prefer the root shared TypeScript configuration. Add a package-level override only for a real
  requirement, such as NodeNext resolution for a Node library or bundler resolution for a frontend.
- Keep generated native binaries out of commits. Commit generated JS/TypeScript declarations or
  other source artifacts when the package's build and distribution workflow requires them.

Changes to exported Rust APIs, the N-API surface, or the remote protocol need a clear
compatibility note in the pull request. Do not assume the embedded and remote interfaces have the
same input constraints.

## Issues and pull requests

Use the bug report form for reproducible defects and the feature request form for proposed work.
Include operating system and architecture, Rust and Node.js versions where relevant, input media
details, reproduction steps, and useful logs. Remove credentials, private URLs, and other secrets
from logs before sharing them.

Pull requests should explain the change and its scope, link a related issue when available, and
include relevant documentation updates. Run the checks that apply to the change and report any
platform-specific checks that could not be run. Keep pull requests focused and avoid presenting
planned capabilities as already implemented.

## Release license inventory

`LICENSE` applies to Remedia. `THIRD_PARTY_NOTICES.md` highlights the major components, including
bundled libopus and native cryptography. `THIRD_PARTY_LICENSES.txt` preserves the full redistribution
texts from the locked Cargo graph and the client's `ws` runtime dependency.
It includes exact source download links for MPL-covered dependencies. Review and refresh this
inventory when dependencies or native build options change; preserve upstream texts verbatim.
Development tools and docs dependencies are not bundled in the npm packages or server binaries.

Package `prepack` scripts copy the root license and notices into each workspace package. The release
packer also stages them into every generated platform package, checks their exact tarball contents,
and builds per-target server archives with all three files. Run `pnpm test:release`; the tarball tests build the
TypeScript packages. These packaging checks do not replace dependency or legal review.
