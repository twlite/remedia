# @remedia/node

This directory contains the Rust N-API crate and private pnpm package for Remedia's Node.js
bindings. It is currently a smoke-test scaffold: the native addon exports `plus_100(input)`, and it
is not connected to `remedia-core` yet.

The intended integration is embedded and in-process: a future N-API API will call native Rust
engine code directly. It is separate from the future standalone remote server. Client-provided
streams and buffers are intended for local bindings initially; neither capability is implemented
in this scaffold.

## Development

Install dependencies from the repository root, then run:

```sh
pnpm --filter @remedia/node test
pnpm --filter @remedia/node build
```

The test command builds the debug addon and runs the Node.js smoke test. Generated native bindings
are build artifacts; the package is private and is not currently published. See the
[Node.js bindings topic](../../apps/docs/content/docs/node/bindings.mdx) for the current status and
planned boundary.
