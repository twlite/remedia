# Remedia

Remedia is an open-source audio streaming engine under development. The project aims to provide a
native Rust media engine, optional direct Node.js bindings, and a standalone server for remote
control.

## Current status

The repository is under active development. `remedia-core` opens local files and HTTP(S) URLs,
decodes them with native FFmpeg libraries, yields normalized PCM, and provides playback sessions
with pause, resume, stop, seeking, generic output, and Opus encoding. `remedia-discord` implements
an outbound voice transport for encoded Opus with voice gateway v8, RTP-size transport encryption,
and DAVE through `davey`. It has not been verified against a live Discord bot yet. Remote RPC,
audio-device output, and the Node.js engine integration are not implemented.

The intended integrations have different boundaries:

- **Embedded Node.js:** `packages/remedia-node` is a minimal N-API scaffold that is intended to call
  the Rust engine directly in the local process. It is intended to accept local inputs such as
  client-provided streams and buffers; the current smoke-test export is not connected to the engine.
- **Remote server:** a future standalone server will expose the engine over a remote protocol.
  Initially, it is intended to accept files and HTTP URLs that the server itself can access.

Remote ingestion of arbitrary client-generated streams is deferred. These are design intentions,
not working features.

## Repository structure

```text
apps/docs/                 Next.js and Fumadocs documentation site
crates/remedia-core/       File/HTTP decoding, playback sessions, PCM output, and Opus encoding
crates/remedia-discord/    Outbound Discord voice connection, DAVE, RTP, and UDP transport
crates/remedia-server/     Intended home of the standalone remote server
packages/remedia-node/     Native Node.js binding scaffold and smoke test
```

The core uses native FFmpeg libraries for media processing; it does not spawn FFmpeg subprocesses.
It outputs interleaved 48 kHz stereo `f32` samples, downmixing through FFmpeg's channel-layout-aware
resampler. Core Opus packets are transport-independent; Discord owns the voice connection, DAVE,
RTP, encryption, UDP, and real-time pacing. See
[`docs/architecture.md`](docs/architecture.md), the [decoding guide](apps/docs/content/docs/decoding.mdx),
the [playback guide](apps/docs/content/docs/playback.mdx), and the
[Discord voice guide](apps/docs/content/docs/discord.mdx) for API details and current limitations.

## Development

You need Rust via rustup, Node.js 22.18 or newer, and pnpm 12.16.1. The workspace declares Rust
1.88 as its minimum supported version and pins Rust 1.99.1 for development. FFmpeg development
headers and libraries for libavformat, libavcodec, libavutil, and libswresample are required to build
`remedia-core`. Building the Opus adapter also requires native libopus development files.

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
