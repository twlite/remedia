# Remedia documentation site

This Next.js and Fumadocs app contains Remedia's user-facing documentation. Pages are grouped by
workspace module under `content/docs/`: `core/` documents decoding and playback, `discord/` covers
the outbound voice transport and bot harness, `node/` documents native bindings, `server/` covers
authenticated WebSocket RPC and deployment, and `observability/` documents shared diagnostics. The
`meta.json` files define titles and navigation order.

From the repository root, run:

```sh
pnpm --filter docs dev
pnpm --filter docs build
pnpm --filter docs start
```

The landing page is `content/docs/index.mdx`. Shared layout and source configuration are under
`src/lib/`.
