# @remedia/plugin-host

A framework-independent operation host with direct in-process dispatch and optional HTTP transport. Remedia does not load provider-specific libraries. Consumers register handlers for their own operation names.

## Direct request objects, without HTTP

```js
import { createPluginHost, createMediaSource } from '@remedia/plugin-host';

const host = createPluginHost({ name: 'Local operations' });
host.register({
  id: 'example.source',
  operations: {
    'source.resolve': async ({ id }) => ({ id, title: 'Example source' }),
    'source.open': ({ id }) => createMediaSource({
      contentType: 'audio/mpeg',
      open: async ({ signal }) => {
        const response = await fetch(`https://media.example.net/${encodeURIComponent(id)}`, { signal });
        if (!response.ok || !response.body) throw new Error('Cannot open source');
        return response.body;
      },
    }),
  },
});

// No HTTP listener or JSON serialization.
const resolved = await host.handle({
  pluginId: 'example.source',
  operation: 'source.resolve',
  input: { id: '123' },
});

const source = await host.handle({
  pluginId: 'example.source',
  operation: 'source.open',
  input: { id: '123' },
});

if (source.handled) {
  const controller = new AbortController();
  const media = await source.result.open({ signal: controller.signal });
  // Pass the stream to the local media consumer.
  await media.cancel?.();
}

await host.close();
```

The direct API is `host.handle({ pluginId, operation, input, signal? })`. `host.execute(pluginId, operation, input, { signal? })` provides a positional equivalent. Both return `{ handled, result? }` and preserve the original value, including class instances, streams, and functions. An `undefined` result means unhandled; `null`, `false`, and other values are handled.

No `publicUrl` or HTTP framework is needed for direct communication. `host.manifest()` provides the registered capabilities in-process.

## Optional HTTP transport

```js
const remote = createPluginHost({
  name: 'Remote operations',
  publicUrl: 'https://plugins.example.com',
  token: process.env.REMEDIA_PLUGIN_TOKEN,
});

// Register the same plugin definitions as local mode.
const response = await remote.fetch(
  new Request('https://plugins.example.com/.well-known/remedia'),
);
```

The HTTP adapter dispatches to the same underlying handlers through `host.handle()`. HTTP requests must carry JSON-compatible values; local calls do not have that restriction.

## Express, Connect, and Node HTTP

```js
import express from 'express';
import { createNodeMiddleware } from '@remedia/plugin-host/node';

const app = express();
app.use(createNodeMiddleware(remote)); // Mount before body parsers, at the origin root.
app.get('/health', (_, res) => res.json({ ok: true }));
app.listen(3000);
```

Unknown paths pass to the next middleware. Plain Node HTTP can use the same adapter. Fetch-based frameworks such as Hono can forward requests to `host.fetch(request)`. The core host never starts a listener.

## HTTP protocol

- `GET /.well-known/remedia` returns the host name, protocol version, and plugin operations.
- `POST /remedia/v1/execute` accepts `{ pluginId, operation, input }` and optionally `Authorization: Bearer ...`. Returns `{ handled: true, result }` or `{ handled: false }`.
- `GET /remedia/v1/media/:token` redeems short-lived, single-use media bytes.

There are no reserved provider or operation names. A registered operation can return a Node/Web stream or a lazy media source constructed with `createMediaSource({ open, contentType })`. For HTTP requests, the host returns a temporary media URL, and invokes a lazy media factory only when the URL is requested. Local calls receive the media object directly.

HTTP request bodies are capped at 64 KiB. Remote media descriptors require `publicUrl`, but local-only hosts do not. The HTTP media endpoint is currently non-seekable and does not support Range requests.

Media URLs act as bearer credentials. Keep services on trusted networks with TLS and appropriate access controls. Plugins run with host-process permissions and are not sandboxed.

## Integration boundaries

The package provides generic registration, execution, lifecycle, and optional transport. Application-specific adapters belong to consuming applications. Rust `remedia-server` discovery, forwarding, media authorization, and end-to-end plugin playback still require integration; they are not provided by this package.
