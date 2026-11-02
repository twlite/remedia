# Remedia

Remedia is an open-source audio streaming engine under development. The project aims to provide a
native Rust media engine, optional direct Node.js bindings, and a standalone server for remote
control.

## Current status

The repository is under active development. `remedia-core` opens local files, HTTP(S) URLs, owned
encoded bytes, and bounded byte streams, decodes them with native FFmpeg libraries, yields
normalized PCM, and provides playback sessions with pause, resume, stop, seeking, generic output,
and Opus encoding. `remedia-discord` implements
an outbound voice transport for encoded Opus with voice gateway v8, RTP-size transport encryption,
and DAVE through `davey`. It has not been verified against a live Discord bot yet.
`packages/remedia-node` provides an initial native Node.js integration for playback, Discord voice
sessions, and structured diagnostics. Remote RPC and local audio-device output are not implemented.

The intended integrations have different boundaries:

- **Embedded Node.js:** `packages/remedia-node` calls the Rust engine directly through N-API. It
  supports file, HTTP(S), Buffer/Uint8Array, and binary Readable playback plus Discord voice
  sessions through an adapter for the application's Discord Gateway client. The application sends
  Gateway payloads and forwards raw dispatch packets; Remedia requests channel joins and manages the
  voice transport lifecycle. Readables feed a bounded native byte queue with backpressure; Remedia
  detaches on cancellation and does not destroy the caller's stream.
- **Remote server:** a future standalone server will expose the engine over a remote protocol.
  Initially, it is intended to accept files and HTTP URLs that the server itself can access.

Remote ingestion of arbitrary client-generated streams is deferred. The Discord transport has not
been verified against a live bot. These are design intentions, not claims that unfinished features
work.

## Repository structure

```text
apps/docs/                    Next.js and Fumadocs documentation site
apps/discord/                 Manual discord.js playback and voice-command harness
crates/remedia-core/          Core decoder, playback, PCM output, and Opus encoding
crates/remedia-discord/       Discord voice transport for encoded Opus
crates/remedia-observability/ Shared tracing, bounded diagnostics, and optional OTLP export
crates/remedia-server/        Future remote server; currently a placeholder crate
packages/remedia-node/        Native N-API bindings and TypeScript wrapper
```

Component guides: [core](crates/remedia-core/README.md),
[Discord transport](crates/remedia-discord/README.md),
[observability](crates/remedia-observability/README.md),
[server](crates/remedia-server/README.md), and [Node.js bindings](packages/remedia-node/README.md).

The core uses native FFmpeg libraries for media processing; it does not spawn FFmpeg subprocesses.
It outputs interleaved 48 kHz stereo `f32` samples, downmixing through FFmpeg's channel-layout-aware
resampler. Playback sessions can apply trusted native FFmpeg audio filtergraphs and independent
linear master gain before output; filter presets are specific to clients such as the Discord test
bot. Core Opus packets are transport-independent; Discord owns the voice connection, DAVE,
RTP, encryption, UDP, and real-time pacing. See
[`docs/architecture.md`](docs/architecture.md), the
[decoding guide](apps/docs/content/docs/core/decoding.mdx),
[playback guide](apps/docs/content/docs/core/playback.mdx), and
[Discord voice guide](apps/docs/content/docs/discord/voice-output.mdx) for API details and current
limitations. The [observability guide](apps/docs/content/docs/observability/configuration.mdx)
covers structured logs, bounded diagnostic routing, and opt-in OTLP traces and metrics. The
[Fumadocs overview](apps/docs/content/docs/index.mdx) groups documentation by workspace module.

## Development

You need Rust via rustup, Node.js 22.18 or newer, and pnpm 12.16.1. The workspace declares Rust
1.88 as its minimum supported version and pins Rust 1.99.1 for development. FFmpeg development
headers and libraries for libavformat, libavcodec, libavutil, libavfilter, and libswresample are
required to build `remedia-core`. Building the Opus adapter also requires native libopus development
files.

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
