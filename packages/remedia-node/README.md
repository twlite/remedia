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
npm install @remedia/node@0.1.0-beta.1
```

Install FFmpeg 7 shared libraries separately before importing the package. npm does not install
FFmpeg or compile the addon. Required ABI majors are avcodec 61, avformat 61, avutil 59,
avfilter 10, and swresample 5. libopus is statically included; FFmpeg is dynamically linked.

Native runtime CI covers Linux x64/ARM64 glibc, macOS x64/ARM64, Windows x64 MSVC, and
FreeBSD x64. Additional build-only packages cover Linux x64/ARM64 musl, Linux ARMv7 hard-float GNU,
Windows x86/ARM64 MSVC, and Android ARM64/ARMv7 (API 24). Build-only targets have no native runtime
integration coverage. See the [distribution guide](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/node/distribution.mdx)
for verification limits and library discovery: `LD_LIBRARY_PATH` on Linux/FreeBSD,
`DYLD_LIBRARY_PATH` on macOS, and DLL directories on `PATH` on Windows. For load errors, check
matching architecture/ABI, transitive FFmpeg dependencies, and that optional npm dependencies were
installed. Windows also needs the Visual C++ v14 runtime.

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
