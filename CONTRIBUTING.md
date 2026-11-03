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
Native target status is tracked separately from the workflow definition; adding a matrix entry does
not by itself establish runtime support:

| Target                                           | Status                                                                                                                  |
| ------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| Linux x86_64 GNU                                 | Native build and runtime tests configured; bundled-Opus CI fixes await a hosted rerun.                                  |
| Linux ARM64 GNU                                  | Native build and runtime tests configured; bundled-Opus CI fixes await a hosted rerun.                                  |
| macOS x86_64                                     | Native build and runtime tests configured; bundled-Opus and LLVM 19 bindgen fixes await a hosted rerun.                 |
| macOS ARM64                                      | Native build and runtime tests configured; bundled-Opus and LLVM 19 bindgen fixes await a hosted rerun.                 |
| Windows x86_64 MSVC                              | Native build and runtime tests configured; bundled-Opus fixes await a hosted rerun.                                     |
| Windows ARM64 MSVC                               | Unsupported for now: the pinned FFmpeg 7 package set does not provide the required Windows ARM64 development libraries. |
| musl, Android, WASI, FreeBSD, and 32-bit targets | Unsupported and not tested.                                                                                             |

The configured native CI matrix uses native runners and tests the same addon artifact with Node.js
22.18, 24, and 26. Promote a target to fully tested support only after its native build and runtime
jobs pass. No target is build-only in the current matrix.

Remedia targets the FFmpeg 7 ABI and dynamically links FFmpeg. Its Opus adapter builds libopus from
the locked bundled source and statically links it into native binaries. FFmpeg executables are used
only for dependency/version checks; Remedia does not spawn them for playback. The CI dependency
specifications are in `.github/native-deps/` and pin FFmpeg 7.1.1 and pkg-config 0.29.2 for Linux
and Windows.

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

The FFmpeg package must include MSVC import libraries, headers, and matching x64 DLLs. Keep the
runtime DLL directory on `PATH` when running tests and applications. MinGW libraries are not
compatible with the MSVC target. Windows builds use MSVC's dynamic CRT to match the bundled Opus
CMake configuration; if the runtime machine does not already have a compatible Microsoft Visual
C++ v14 runtime, install the current Visual C++ Redistributable. Remedia does not bundle that
runtime, and the private N-API package does not promise static CRT linkage.

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
Cargo. Installing a prebuilt platform addon from a
future published package would not need those build tools and does not install FFmpeg during package
installation. The current repository package is private, and no FFmpeg or native compilation is
attached to npm/pnpm install hooks.
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
