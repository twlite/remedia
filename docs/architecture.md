# Architecture

This document describes Remedia's intended design. The current crates are starter code and do not
yet implement media playback, Discord transport, remote RPC, or Node.js bindings.

## Rust crates

- **`remedia-core`** is intended to own decoding, playback state, seeking, filtering, and media
  processing. It should use native FFmpeg libraries rather than starting FFmpeg subprocesses.
- **`remedia-discord`** is intended to adapt the generic engine's output to Discord voice transport.
  Discord-specific behavior should remain outside the generic media engine.
- **`remedia-server`** is intended to run the engine as a standalone remote service and expose
  operations over a WebSocket RPC protocol.
- **`remedia-node`** is a planned N-API package. It should call Rust functionality directly in the
  Node.js process instead of routing local calls through the standalone server.

These boundaries are goals, not existing APIs. Add crates or packages only when implementation
needs them.

## Embedded and remote integration

Embedded Node.js integration is intended for local applications that need direct native access.
It may ingest client-provided streams or buffers because the binding and the producer run in the
same process.

Remote integration runs in a separate server process. Its initial inputs are files and HTTP URLs
that the server can access. Uploading arbitrary client-generated binary streams to the remote
server is intentionally deferred; it adds a transport and lifecycle surface that local bindings do
not need.

## Processing and inputs

The initial input focus is local files and HTTP URLs. Native FFmpeg libraries are intended to handle
media decoding and processing. Rust should perform this work on native threads or asynchronous
workers appropriate to the implementation, keeping blocking media work off an async executor's
control path where necessary.

No crate currently declares FFmpeg dependencies, links native FFmpeg libraries, or starts media
workers. CI therefore does not install FFmpeg development packages today. When a crate begins
linking FFmpeg, its build requirements should be documented and installed in CI rather than hidden
by disabling that crate's checks.
