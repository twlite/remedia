# Remedia

Remedia is an open-source audio streaming engine under development. The project aims to provide a
native Rust media engine, optional direct Node.js bindings, and a standalone server for remote
control.

## Current status

The repository is under active development. `remedia-core` opens local files, HTTP(S) URLs, owned
encoded bytes, and bounded byte streams, decodes them with native FFmpeg libraries, yields
normalized PCM, and provides playback sessions with pause, resume, stop, seeking, generic output,
and Opus encoding. When an Opus-capable output receives a compatible untrimmed Opus source with no
active PCM processing, playback can forward source packets directly and switch to or from native
PCM processing without reopening the input. `remedia-discord` implements
an outbound voice transport for encoded Opus with voice gateway v8, RTP-size transport encryption,
and DAVE through `davey`. Live playback through a configured Discord bot has been exercised; CI
uses deterministic tests and does not connect to Discord.
`packages/remedia-node` provides an initial native Node.js integration for playback, Discord voice
sessions, and structured diagnostics. `crates/remedia-server` provides authenticated, in-memory
WebSocket RPC over the same native Rust crates. Its v0 protocol supports node statistics, resumable
client sessions, guild players, Discord voice transports using credentials obtained by the bot's
Gateway client, and playback controls. `packages/remedia-client` is a pure TypeScript orchestration
SDK that selects between direct embedded N-API nodes and remote server nodes, with sticky guild
routing and remote session resumption. It does not migrate active playback across nodes. The server
is a single independent node and does not use N-API or coordinate with other servers. Remote-only
applications do not need the client's optional `@remedia/node` peer dependency.

The intended integrations have different boundaries:

- **Embedded Node.js:** `packages/remedia-node` calls the Rust engine directly through N-API. It
  supports file, HTTP(S), Buffer/Uint8Array, and binary Readable playback plus Discord voice
  sessions through an adapter for the application's Discord Gateway client. The application sends
  Gateway payloads and forwards raw dispatch packets; Remedia requests channel joins and manages the
  voice transport lifecycle. Readables feed a bounded native byte queue with backpressure; Remedia
  detaches on cancellation and does not destroy the caller's stream.
- **Remote server:** `remedia-server` accepts authenticated JSON RPC at `/v1/ws`. It opens
  server-accessible HTTP(S) URLs and local files restricted to configured roots. It does not accept
  remote byte uploads, send Discord Gateway OP 4, or persist sessions through process restarts.

Remote ingestion of arbitrary client-generated streams is deferred. The manual
[voice region recovery procedure](/docs/discord/bot-harness#manual-voice-region-recovery) covers
transport replacement in embedded and remote modes; automated CI does not use Discord
credentials. These are design intentions, not claims that every planned component exists.

## Repository structure

```text
apps/docs/                    Next.js and Fumadocs documentation site
apps/discord/                 Manual discord.js playback and voice-command harness
crates/remedia-core/          Core decoder, playback, PCM output, and Opus encoding
crates/remedia-discord/       Discord voice transport for encoded Opus
crates/remedia-observability/ Shared tracing, bounded diagnostics, and optional OTLP export
crates/remedia-server/        Authenticated standalone WebSocket RPC server
packages/remedia-node/        Native N-API bindings and TypeScript wrapper
packages/remedia-client/     Hybrid embedded/remote TypeScript orchestration SDK
```

Component guides: [core](crates/remedia-core/README.md),
[Discord transport](crates/remedia-discord/README.md),
[observability](crates/remedia-observability/README.md),
[server](crates/remedia-server/README.md), [Node.js bindings](packages/remedia-node/README.md),
and the [hybrid TypeScript client](packages/remedia-client/README.md).
The server's full configuration and protocol references are also available in the
[Fumadocs server guide](apps/docs/content/docs/server/overview.mdx).

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

## Installation

The upcoming public prerelease is `0.1.0-beta.1`. Both packages require Node.js **22.18.0 or newer**
and expose ESM entrypoints.

```sh
# Embedded engine (install FFmpeg 7 shared libraries separately first)
npm install @remedia/node
# Remote-only SDK (no local addon or FFmpeg required)
npm install @remedia/client
# Hybrid/embedded SDK applications need both packages
npm install @remedia/client @remedia/node
```

Installing `@remedia/node` does not install FFmpeg. The standalone server also needs independently
installed FFmpeg 7 libraries on its host. Neither distribution bundles FFmpeg.
See the [distribution guide](apps/docs/content/docs/node/distribution.mdx) for native targets,
runtime library discovery, and load-error troubleshooting.

```js
import { Remedia } from '@remedia/node';

const remedia = await Remedia.create();
try {
  const playback = remedia.createPlaybackSession();
  await playback.play({ file: './audio.wav' });
  console.log(await playback.wait());
} finally {
  await remedia.close();
}
```

This standalone session decodes into a null output; it does not play through local speakers.
See the [client README](packages/remedia-client/README.md) for remote Discord integration.

## Development

You need Rust via rustup, Node.js 22.18 or newer, and pnpm 12.16.1. The workspace declares Rust
1.88 as its minimum supported version and pins Rust 1.99.1 for development. FFmpeg development
headers and libraries for libavformat, libavcodec, libavutil, libavfilter, and libswresample are
required to build `remedia-core`. The Opus adapter builds libopus from bundled source and links it
statically, so a separate system libopus development package is not required.

```sh
pnpm install --frozen-lockfile
pnpm lint
pnpm typecheck
pnpm format:check
pnpm --filter @remedia/node test
pnpm --filter @remedia/client test
pnpm --filter discord-app test

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cargo run -p remedia-core --example decode -- file ./path/to/audio.wav
cargo run -p remedia-core --example decode -- url https://example.com/audio.mp3
cargo run -p remedia-core --example playback -- file ./path/to/audio.wav
cargo run -p remedia-server -- --config ./remedia.config.json
```

The server requires a strict `remedia.config.json`; see
[`crates/remedia-server/remedia.config.example.json`](crates/remedia-server/remedia.config.example.json).
It binds to loopback by default. Put remote deployments behind TLS termination and restrict
outbound network access for remote media URLs.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup details, platform notes, and pull request
guidance.

## License and media responsibility

Remedia is a general-purpose media processing and transport engine. It does not provide media
content or grant rights to access or use third-party media. Developers and operators are responsible
for ensuring their use complies with applicable laws, intellectual property rights, and third-party
service terms.

Remedia is provided under the [MIT License](LICENSE) without warranty. Dependency terms remain
separate; see [third-party notices](THIRD_PARTY_NOTICES.md), including bundled libopus and
independently installed FFmpeg.
