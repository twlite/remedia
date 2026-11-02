# remedia-server

`remedia-server` is reserved for Remedia's future standalone remote server. The current crate is
only a placeholder: running it prints `Hello, world!` and exits. It does not expose RPC, WebSocket,
HTTP, playback, or audio streaming functionality yet.

The intended remote integration will run the engine in the server process and initially accept
files and HTTP(S) URLs that the server itself can access. Client-generated streams and buffers are
intended for local Node.js bindings first. These are design intentions, not implemented behavior.

## Current crate

From the repository root, run the placeholder with:

```sh
cargo run -p remedia-server
```

See the [remote server topic](../../apps/docs/content/docs/server/overview.mdx) and
[`docs/architecture.md`](../../docs/architecture.md) for the intended boundary and current status.
