# @remedia/node

This directory contains the Rust N-API crate and private pnpm package for Remedia's embedded Node.js
bindings. The addon calls `remedia-core`, `remedia-discord`, and `remedia-observability` directly in
the Node process; it does not use `remedia-server` as an intermediary.

The TypeScript entrypoint exports `Remedia`, `PlaybackSession`, and `DiscordSession`. Playback
accepts local files, HTTP(S) URLs, in-memory `Buffer`/`Uint8Array` media, and binary Node.js
`Readable` streams. Standalone playback currently decodes into a null output; Discord sessions
encode PCM to Opus in `remedia-core` and send packets through the existing voice transport. The
application must obtain Discord voice session data through its own Gateway client.

Each `Remedia` owns an isolated observability context. Rust diagnostics are forwarded through a
bounded callback queue with `on('debug', listener)`; use `close()` to stop sessions, disconnect
transports, and shut down owned telemetry. Operational errors reject API Promises and remain
available on session state independently of diagnostic delivery.

## Development

Install dependencies from the repository root, then run:

```sh
pnpm --filter @remedia/node test
pnpm --filter @remedia/node build
```

The test command builds the debug addon and runs local Node.js integration tests. `build` creates a
release addon and TypeScript declarations. N-API loader/declaration files are generated from the
Rust source by `@napi-rs/cli`; Vitest tests live in `tests/` and are grouped by behavior. Rust code
belongs in `src/`, TypeScript library code in `lib/`, and `dist/` is generated output. The package
is private and is not published. See the
[Node.js bindings topic](../../apps/docs/content/docs/node/bindings.mdx) for the API and current
limitations.
