# Remedia

Remedia is an open-source audio streaming engine under development. The project aims to provide a
native Rust media engine, optional direct Node.js bindings, and a standalone server for remote
control.

## Current status

The repository is under active development. `remedia-core` now opens local files and HTTP(S) URLs,
selects an audio stream, decodes it with native FFmpeg libraries, and yields normalized PCM frames.
Core playback sessions now manage pause, resume, stop, seek, position, and generic PCM output.
Discord transport, remote RPC, and actual audio-device output are not implemented yet. The Node.js
package still contains only a napi-rs smoke binding and is not connected to `remedia-core`.

The intended integrations have different boundaries:

- **Embedded Node.js:** `packages/remedia-node` is a minimal N-API scaffold that will call the Rust
  engine directly in the local process. It is intended to accept local inputs such as
  client-provided streams and buffers; the current smoke-test export is not connected to the engine.
- **Remote server:** a future standalone server will expose the engine over a remote protocol.
  Initially, it is intended to accept files and HTTP URLs that the server itself can access.

Remote ingestion of arbitrary client-generated streams is deferred. These are design intentions,
not working features.

## Repository structure

```text
apps/docs/                 Next.js and Fumadocs documentation site
crates/remedia-core/       File/HTTP decoding, playback sessions, and generic PCM output
crates/remedia-discord/    Intended home of Discord voice transport
crates/remedia-server/     Intended home of the standalone remote server
packages/remedia-node/     Native Node.js binding scaffold and smoke test
```

The core uses native FFmpeg libraries for media processing; it does not spawn FFmpeg subprocesses.
It outputs interleaved 48 kHz stereo `f32` samples, downmixing through FFmpeg's channel-layout-aware
resampler. Discord transport will remain separate from the generic engine. See
[`docs/architecture.md`](docs/architecture.md), the [decoding guide](apps/docs/content/docs/decoding.mdx),
and the [playback guide](apps/docs/content/docs/playback.mdx) for API details and current limitations.

## Development

You need Rust via rustup, Node.js 22.18 or newer, and pnpm 12.16.1. The workspace declares Rust
1.88 as its minimum supported version and pins Rust 1.99.1 for development. FFmpeg development
headers and libraries for libavformat, libavcodec, libavutil, and libswresample are required to build
`remedia-core`.

```sh
pnpm install --frozen-lockfile
pnpm lint
pnpm typecheck
pnpm format:check
pnpm --filter @remedia/node test

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cargo run -p remedia-core --example decode -- file ./path/to/audio.wav
cargo run -p remedia-core --example decode -- url https://example.com/audio.mp3
cargo run -p remedia-core --example playback -- file ./path/to/audio.wav
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup details, platform notes, and pull request
guidance.
