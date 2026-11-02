# Remedia

Remedia is an open-source audio streaming engine under development. The project aims to provide a
native Rust media engine, optional direct Node.js bindings, and a standalone server for remote
control.

## Current status

The repository is at an early stage. The three engine/server crates currently contain starter code;
decoding, playback, Discord transport, and remote RPC are not implemented yet. The Node.js package
contains a minimal napi-rs smoke binding and is not connected to `remedia-core`. The `apps/docs`
directory contains a Next.js and Fumadocs starter site.

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
apps/docs/                 Next.js documentation site
crates/remedia-core/       Intended home of the generic Rust media engine
crates/remedia-discord/    Intended home of Discord voice transport
crates/remedia-server/     Intended home of the standalone remote server
packages/remedia-node/     Native Node.js binding scaffold and smoke test
```

The core is intended to use native FFmpeg libraries for media processing, rather than spawning
FFmpeg subprocesses. Discord transport will remain separate from the generic engine. See
[`docs/architecture.md`](docs/architecture.md) for the intended boundaries and current limitations.

## Development

You need Rust via rustup, Node.js 22.18 or newer, and pnpm 12.16.1. The workspace declares Rust
1.88 as its minimum supported version and pins Rust 1.99.1 for development. FFmpeg development
libraries are not required by the current starter crates.

```sh
pnpm install --frozen-lockfile
pnpm lint
pnpm typecheck
pnpm format:check
pnpm --filter @remedia/node test

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup details, platform notes, and pull request
guidance.
