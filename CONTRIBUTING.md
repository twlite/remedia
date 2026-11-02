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

`remedia-core` links native FFmpeg libraries through `ffmpeg-next` and `ffmpeg-sys-next`, and its
Opus adapter links native libopus, and playback filters use libavfilter. Install development headers
for libavformat, libavcodec, libavutil, libavfilter, libswresample, and libopus, plus `pkg-config`
and Clang/libclang for the bindings build.
Common starting points are:

- macOS: install Xcode Command Line Tools, then run `brew install ffmpeg opus pkg-config`.
- Debian or Ubuntu:

  ```sh
  sudo apt install clang libclang-dev pkg-config libavcodec-dev libavfilter-dev libavformat-dev libavutil-dev libswresample-dev libopus-dev
  ```

On Windows, install FFmpeg development libraries that match the target architecture and configure
`FFMPEG_DIR` or `PKG_CONFIG_PATH` so `ffmpeg-sys-next` can find their headers and libraries. Make
libclang discoverable to bindgen as well. FFmpeg's executable is not used by Remedia at runtime.

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

cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The docs app can be run with `pnpm --filter docs dev` and built with
`pnpm --filter docs build`. `pnpm --filter @remedia/node test` builds the debug N-API addon and runs
the TypeScript integration tests with Vitest. Add tests under `packages/remedia-node/tests/` and
split them by behavior; `typecheck` checks both library and test TypeScript. The package keeps Rust
code in `src/`, TypeScript library code in `lib/`, and generated TypeScript output in `dist/`.
The Discord harness runs TypeScript directly with `tsx`; its `typecheck` script uses `tsc --noEmit`,
and its local queue tests run with Node's built-in test runner. To start the bot, run
`pnpm --filter discord-app start`. This builds the native Node package first and reads
`DISCORD_TOKEN` and `APPLICATION_ID` from `apps/discord/.env`.
`cargo test -p remedia-core` runs deterministic file, in-memory, streaming, and local-HTTP decoder tests. The
core tests cover decoder conversion, seeking, playback lifecycle, native FFmpeg filtergraph processing,
master gain, Opus framing, backpressure, and stop cleanup. `remedia-discord` tests parsing, fail-closed DAVE behavior, RTP, transport encryption,
UDP discovery, pacing, and bounded queue cancellation without Discord credentials. The
examples can be run with `cargo run -p remedia-core --example decode -- file <path>`,
`cargo run -p remedia-core --example decode -- url <http(s)-url>`, or
`cargo run -p remedia-core --example playback -- file <path>`.
The optional manual voice sender example is `cargo run -p remedia-discord --example play_file`; it
requires externally obtained Discord voice session information and a reachable voice endpoint.

## Where code belongs

- Add a Rust crate under `crates/` when it has an independently useful boundary in the engine,
  transport, or server. `remedia-core` owns source opening, decoding, resampling, playback sessions,
  generic PCM output, and transport-independent Opus encoding. `remedia-discord` owns Discord voice
  connections, DAVE, RTP, encryption, UDP, and packet scheduling. Add crates to the Cargo workspace
  only when they exist and are needed.
- Add a JavaScript or TypeScript package under `packages/` when it has a distinct package boundary.
  Keep an N-API crate alongside its owning Node package: `packages/remedia-node` is included in the
  root Cargo workspace and calls the Remedia engine in process. Keep its Rust files in `src/` and
  TypeScript files in `lib/`; do not mix languages in `src/`.
- Keep documentation tooling and site code under `apps/docs/`.
- Prefer the root shared TypeScript configuration. Add a package-level override only for a real
  requirement, such as NodeNext resolution for a Node library or bundler resolution for a frontend.
- Keep generated native binaries out of commits. Commit generated JS/TypeScript declarations or
  other source artifacts when the package's build and distribution workflow requires them.

Changes to exported Rust APIs, the planned N-API surface, or the remote protocol need a clear
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
