# @remedia/plugin-host

Experimental HTTP plugin-host foundation for Remedia. It is framework independent: `host.fetch(request)` uses the standard Fetch API, while `createNodeMiddleware(host)` accepts ordinary Node HTTP, Express, and Connect requests. This is a **protocol foundation only**. The Rust server does not yet perform plugin discovery or playback through plugin hosts, and legacy discord-player extractor registration is not integrated.

## Express or Connect

```js
import express from 'express';
import { createPluginHost } from '@remedia/plugin-host';
import { createNodeMiddleware } from '@remedia/plugin-host/node';

const host = createPluginHost({
  name: 'Media plugins',
  publicUrl: 'http://127.0.0.1:3000',
  token: process.env.REMEDIA_PLUGIN_TOKEN,
});
host.register({
  id: 'example.search',
  operations: {
    'search.query.resolve': async ({ query }) => ({ tracks: [{ title: query }] }),
  },
});

const app = express();
// Must be before body-parsing middleware.
app.use(createNodeMiddleware(host));
app.listen(3000);
```

For plain Node HTTP, pass `createNodeMiddleware(host)` to `createServer`. For Fetch-based routers such as Hono, forward the incoming `Request` to `host.fetch(request)`.

## Endpoints

- `GET /.well-known/remedia`: host manifest, protocol version and available operations.
- `POST /remedia/v1/execute`: `{ pluginId, operation, input }` with optional `Authorization: Bearer <token>`. Returns `{ handled: true, result }` or `{ handled: false }`.
- `GET /remedia/v1/media/:token`: one-use, short-lived encoded media stream when a `track.stream` handler returns a Node or Web readable stream.

The execution request body is capped at 64 KiB. Media tokens are random bearer capabilities, have a bounded TTL and expire if never consumed. Streaming is currently non-seekable. The media URL is not protected by the execute endpoint's Bearer token, so keep the host on a private network or enforce network policy and TLS in front of it. Avoid logging media token URLs.

Do not host untrusted plugins within this process. The package does not sandbox plugin code. The `.well-known/remedia` suffix is a proposed convention, not a registered standard.

## Remaining integration

Implement Rust-side host discovery, authenticated operation forwarding, SSRF-safe URL authorization, v8 plugin and legacy extractor adapters, node capability discovery, and remote playback. Those are not implemented by this package.
