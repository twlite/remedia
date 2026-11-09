# @remedia/plugin-host

Framework-independent, experimental plugin operation host for Remedia. Remedia does not load any provider-specific libraries. Consumers register operations using their own application-level adapters.

The same registration supports direct invocation and optional HTTP access. Local calls do not create an HTTP server, serialize their arguments, or round-trip through Fetch.

## Direct invocation (no server)

```js
import { createPluginHost } from '@remedia/plugin-host';

const host = createPluginHost({ name: 'Sources' });
host.register({
  id: 'example.catalog',
  operations: {
    lookup: async ({ id }, { signal }) => ({ id, title: 'Example audio' }),
  },
});

const { handled, result } = await host.execute('example.catalog', 'lookup', { id: '123' });
await host.close();
```

An external integration can register any operation names it needs. Remedia does not know about music search, particular audio providers, or application-specific track classes.

## HTTP using Fetch

```js
const host = createPluginHost({
  name: 'Media services',
  publicUrl: 'http://127.0.0.1:3000',
  token: process.env.REMEDIA_PLUGIN_TOKEN,
});

// Standard Request -> Response; forward using a Fetch-based framework
const response = await host.fetch(new Request('http://127.0.0.1:3000/.well-known/remedia'));
```

A host has `manifest()` for in-process discovery and `matches(path)` for recognizing protocol routes. `fetch()` does not listen on a port.

## Express, Connect, and Node HTTP

```js
import express from 'express';
import { createNodeMiddleware } from '@remedia/plugin-host/node';

const app = express();
app.use(createNodeMiddleware(host)); // Before body parsers, mounted at the origin root
app.get('/health', (_, res) => res.json({ ok: true }));
app.listen(3000);
```

Unrecognized paths fall through to the next middleware. Plain Node HTTP can use the same adapter. Frameworks built on the standard Fetch API, including Hono, may forward Requests to `host.fetch` or use their own router adapter.

## HTTP protocol

- `GET /.well-known/remedia`: host name, version, and registered capabilities.
- `POST /remedia/v1/execute`: `{ "pluginId": "...", "operation": "...", "input": ... }` with optional Bearer authentication. Returns `{ "handled": true, "result": ... }` or `{ "handled": false }`.
- `GET /remedia/v1/media/:token`: a short-lived, one-use stream of encoded media bytes.

Only JSON-compatible values can cross the HTTP boundary. Direct `execute()` returns handler results untouched, including functions, streams, and class instances. An operation may return any Node/Web readable or a lazy media source created via `createMediaSource({ open: ({ signal }) => readable })`. The lazy source's `open()` runs when the media URL is fetched, not when its descriptor is generated.

HTTP execute requests have a 64 KiB body limit. Remote media descriptors require `publicUrl`; local-only hosts do not. The one-use media endpoint is non-seekable and has no Range support yet.

Media URLs are bearer capabilities and must not be logged or exposed to untrusted callers. Operate on a trusted network with TLS and explicit outbound access controls. The host does not sandbox plugins and optional HTTP authentication is not a substitute for isolation.

The `remedia` well-known URI suffix is currently unregistered; RFC 8615 requires registration for formally defined well-known URIs. Registration should follow a stable public protocol specification.

## Scope

This package is a generic host, **not a provider loader**. Provider/extractor adapters belong in consuming applications. Native `remedia-server` discovery, authenticated HTTP operation forwarding, media URL authorization, and end-to-end playback are separate integration work and not yet present in this PR.
