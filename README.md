# Remedia

Remedia is a Rust-first audio engine with direct Node.js bindings and an optional standalone
WebSocket server. It uses native FFmpeg libraries for media inspection and decoding, provides
playback controls and filtering, and supports Discord voice output.

The project is under active development. Remedia handles audio; applications provide media
sources, platform metadata, and their existing Discord Gateway connection.

## Packages

| Component                                    | Purpose                                                                         |
| -------------------------------------------- | ------------------------------------------------------------------------------- |
| [`remedia-core`](crates/remedia-core)        | Media probing, decoding, playback, filters, and generic audio output            |
| [`@remedia/node`](packages/remedia-node)     | Direct, in-process Node.js bindings with file, URL, buffer, and Readable inputs |
| [`remedia-server`](crates/remedia-server)    | Authenticated WebSocket RPC for server-accessible URLs and allowed local files  |
| [`@remedia/client`](packages/remedia-client) | One TypeScript client for embedded and remote nodes                             |
| [`remedia-discord`](crates/remedia-discord)  | Discord voice transport, encryption, and packet scheduling                      |

Embedded execution does not run a local server. Remote execution does not accept byte uploads or
client-owned Readables. Applications provide source integrations. The independent [`@remedia/plugin-host`](packages/remedia-plugin-host)
package supplies generic direct dispatch and optional HTTP media delivery; Rust server plugin playback
integration is not implemented. Remedia does not implement the Lavalink API.

## Installation

Node.js **22.18 or newer** is required. Packages expose ESM entrypoints.

```sh
# Embedded execution
npm install @remedia/node

# Remote client
npm install @remedia/client

# Hybrid execution
npm install @remedia/client @remedia/node
```

Install **FFmpeg 9 shared libraries** separately on hosts running the native addon or standalone
server. They are not bundled. Remote-only clients do not need FFmpeg or the native addon.
See [installing FFmpeg 9](apps/docs/content/docs/node/ffmpeg-9.mdx) and the [distribution guide](apps/docs/content/docs/node/distribution.mdx) for supported targets.

## Example

```ts
import { Remedia } from '@remedia/node';

const remedia = await Remedia.create();
try {
  const info = await remedia.probe({ file: './audio.mp3' });
  console.log(info.durationMs, info.metadata.title);

  // Resolve a fresh source for playback; probing retains no reusable input.
  const playback = remedia.createPlaybackSession();
  await playback.play({ file: './audio.mp3' });
  console.log(await playback.wait());
} finally {
  await remedia.close();
}
```

This standalone playback session discards audio through a null output. It does not play through
local speakers. Probing creates no playback session; Readables passed to it may be partially or
fully consumed and should be discarded. Unknown metadata stays unknown.

## Documentation

- [Core media APIs](apps/docs/content/docs/core/decoding.mdx) and [playback](apps/docs/content/docs/core/playback.mdx)
- [Node.js bindings](apps/docs/content/docs/node/bindings.mdx)
- [Hybrid TypeScript client](apps/docs/content/docs/client/overview.mdx)
- [Server setup](apps/docs/content/docs/server/overview.mdx) and [RPC protocol](apps/docs/content/docs/server/protocol.mdx)
- [Discord voice integration](apps/docs/content/docs/discord/voice-output.mdx)
- [Diagnostics and telemetry](apps/docs/content/docs/observability/configuration.mdx)

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for toolchains, native dependencies, tests, and release
workflows. The workspace uses Rust and pnpm; user-facing documentation lives in `apps/docs`.

## License

[MIT](LICENSE). See [third-party notices](THIRD_PARTY_NOTICES.md) for dependency terms.
Applications are responsible for obtaining authorized media sources.
