# @remedia/node

This directory contains the Rust N-API crate and public npm package for Remedia's embedded Node.js
bindings. The addon calls `remedia-core`, `remedia-discord`, and `remedia-observability` directly in
the Node process; it does not use `remedia-server` as an intermediary.

The TypeScript entrypoint exports `Remedia`, `PlaybackSession`, and `DiscordSession`. Playback
accepts local files, HTTP(S) URLs, in-memory `Buffer`/`Uint8Array` media, and binary Node.js
`Readable` streams. Standalone playback currently decodes into a null output; Discord sessions
encode PCM to Opus in `remedia-core` and send packets through the existing voice transport. The
application supplies its Discord Gateway send function and forwards raw Gateway packets to
`Remedia.receive()`; the TypeScript adapter handles OP 4 and collects matching voice dispatches
before opening the native transport. Session tokens and endpoints remain internal details.

Each `Remedia` owns an isolated observability context. Rust diagnostics are forwarded through a
bounded callback queue with `on('debug', listener)`; use `close()` to stop sessions, disconnect
transports, and shut down owned telemetry. Operational errors reject API Promises and remain
available on session state independently of diagnostic delivery.

Playback and voice sessions are typed Node `EventEmitter`s. Subscribe with exported
`PlaybackSessionEvents` and `VoiceSessionEvents` constants; `Remedia` forwards session lifecycle
events and provides `RemediaEvents`. `playbackStarted` means native processing began, while
`playbackCompleted` means natural EOF was processed, not that Discord buffers drained. Events are
generation-correlated. `play(source, { customMetadata })` keeps arbitrary application metadata in
JavaScript and includes it in that generation's lifecycle events. Ordered `hooks.beforePlay()`
middleware can cancel or reject a start before native playback. See the
[Node.js binding guide](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/node/bindings.mdx) for payloads and semantics.

## Installation

Requires Node.js **22.18.0 or newer** (ESM).

```sh
npm install @remedia/node
```

Install FFmpeg 9 shared libraries separately on the Node.js host. npm does not install FFmpeg, and
importing the package never downloads anything. The Remedia installer provisions FFmpeg 9.0.2 only
(no server) into `~/.remedia/ffmpeg9`, which the package finds automatically:

```sh
curl -fsSL https://raw.githubusercontent.com/twlite/remedia/main/scripts/install.sh | sh -s -- --ffmpeg-only
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/twlite/remedia/main/scripts/install.ps1))) -FFmpegOnly
```

No `LD_LIBRARY_PATH`, `DYLD_LIBRARY_PATH`, `PATH` change, or launcher is needed. Discovery order:
`REMEDIA_FFMPEG_DIR` (an explicit override that is never silently replaced), an existing system
FFmpeg 9, then `~/.remedia/ffmpeg9`. Required ABI majors are avcodec 63, avformat 63, avutil 61,
avfilter 12, and swresample 7. `getFfmpegRuntime()` reports which installation was loaded, and a
failed import lists the paths considered. libopus is statically included; FFmpeg is dynamically linked.
FFmpeg is needed on the Node.js host in embedded mode; in remote mode it is needed on the
`remedia-server` host instead.

Native runtime CI covers Linux x64/ARM64 glibc, macOS x64/ARM64, Windows x64 MSVC, and
FreeBSD x64. Additional build-only packages cover Linux x64/ARM64 musl, Linux ARMv7 hard-float GNU,
Windows x86/ARM64 MSVC, and Android ARM64/ARMv7 (API 24). Build-only targets have no native runtime
integration coverage. See the [distribution guide](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/node/distribution.mdx)
and [native library troubleshooting](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/node/troubleshooting.mdx)
for verification limits and load errors. Windows also needs the Visual C++ v14 runtime.

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

## Development

Install dependencies from the repository root, then run:

```sh
pnpm --filter @remedia/node test
pnpm --filter @remedia/node build
```

The test command builds the debug addon and runs local Node.js integration tests. `build` creates a
release addon and TypeScript declarations. N-API loader/declaration files are generated from the
Rust source by `@napi-rs/cli`; Vitest tests live in `tests/` and are grouped by behavior. Rust code
belongs in `src/`, TypeScript library code in `lib/`, and `dist/` is generated output. See the
[Node.js bindings topic](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/node/bindings.mdx) for the API and current
limitations.

## License and media responsibility

Remedia is a general-purpose media processing and transport engine. It does not provide media
content or grant rights to access or use third-party media. Developers and operators are responsible
for ensuring their use complies with applicable laws, intellectual property rights, and third-party
service terms.

Remedia is provided under the [MIT License](./LICENSE) without warranty. Dependency terms remain
separate; see [third-party notices](./THIRD_PARTY_NOTICES.md), including bundled libopus and
independently installed FFmpeg.

## Session configuration and channel movement

A client owns one session per guild. Separate clients (including separate bot users) can use the
same guild and channel with independent playback, transport generations, and events. `userId` is
the Discord bot user ID, not a Remedia client session ID. Channel IDs are mutable destinations.
Duplicate guild registration on the same client throws; dispose the old session before replacing it.

```ts
const session = remedia.createDiscordSession({
  guildId,
  channelId,
  userId,
  selfDeaf: true,
  selfMute: false,
  volume: 0.75,
  filterGraph: 'bass=gain=3',
});
await session.connect();
await session.move(otherChannelId);
await session.setSelfDeaf(false);
await session.setSelfMute(true);
await session.dispose();
```

`selfDeaf` defaults to `true` (previously joins always sent `false`); `selfMute` defaults to
`false`. These flags affect Discord voice state, independently of playback pause and gain.
Setters retain values before connection and update Gateway OP 4 after joining. Unchanged values
send nothing. Joins, credential refreshes, and moves preserve the flags. Keep the library-agnostic
`send(guildId, payload)` and `receive(payload)` Gateway integration wired throughout the session.

Initial `volume` is finite linear gain from 0 through 2, default 1. `filterGraph` is a trusted raw
FFmpeg audio filtergraph or `null` (default). Invalid gain and empty/non-string graphs are rejected
at creation; native FFmpeg validates graph syntax when the backend is allocated/configured, before
playback is allowed. Configuration is serialized with runtime setters and applied before audio
starts. Both values persist across tracks and transport replacement; existing `setVolume()` and
`filters.set()`/`clear()` remain available with their existing change events.

`move(channelId): Promise<void>` requests fresh Gateway credentials and replaces the existing voice
transport without restarting or seeking playback. Decoder input (including one-shot Readables),
filters, gain, playback generation, and paused state are retained. A move to the current destination
shares a pending connection or resolves immediately when connected. A newer destination supersedes
a pending attempt with an `AbortError`; `connect()` shares the latest attempt. Moving while explicit
disconnection or disposal is underway rejects. `disconnect()`/`dispose()` cancel a pending move.
`channelId` reports the latest requested destination, including after a failed move.

Intentional moves do not use the automatic recovery retry policy. A failed move reports a failed
voice state and leaves playback parked with bounded buffering; call `connect()` to retry the
requested destination, `move()` to choose another, or `disconnect()`/`dispose()` to release it.
Ordinary transport recovery retains its existing bounded credential-refresh retries. Transport
replacement itself does not emit playback stopped/completed/failed events.

Gateway mocks verify orchestration and local native tests verify media lifecycles. Real Discord
channel movement, same-channel multi-bot voice, credential ordering, and self flags still require
an application integration test with Discord. Forward only each bot's own Gateway dispatch stream.
