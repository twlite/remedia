import { createServer, request as nodeRequest } from 'node:http';
import { once } from 'node:events';
import type { AddressInfo } from 'node:net';
import { afterEach, expect, it, vi } from 'vitest';
import { createMediaSource, createPluginHost } from '../src/index.js';
import { createNodeMiddleware } from '../src/node.js';
const cleanup: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  await Promise.all(cleanup.splice(0).map((f) => f()));
});
async function service() {
  const host = createPluginHost({ name: 'Node', publicUrl: 'http://localhost', onError: vi.fn() });
  const middleware = createNodeMiddleware(host);
  const server = createServer((req, res) => {
    void middleware(req, res, (error) => {
      res.statusCode = error ? 500 : 200;
      res.end(error ? 'error' : 'fallthrough');
    });
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  cleanup.push(async () => {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => server.close((e) => (e ? reject(e) : resolve())));
    await host.close();
  });
  return { host, origin };
}
it('handles raw Connect/Express request bodies and falls through unknown paths', async () => {
  const { host, origin } = await service();
  host.register({ id: 'example', operations: { echo: (input) => input } });
  expect(await (await fetch(origin + '/health')).text()).toBe('fallthrough');
  expect((await fetch(origin + '/.well-known/remedia')).status).toBe(200);
  const response = await fetch(origin + '/remedia/v1/execute', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ pluginId: 'example', operation: 'echo', input: { id: 42 } }),
  });
  expect(await response.json()).toEqual({ handled: true, result: { id: 42 } });
});
it('rejects consumed bodies with an actionable error', async () => {
  const host = createPluginHost({ name: 'Parsed' });
  const middleware = createNodeMiddleware(host);
  const server = createServer((req, res) => {
    Object.assign(req, { body: {} });
    void middleware(req, res);
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  cleanup.push(async () => {
    server.closeAllConnections();
    await new Promise<void>((r) => server.close(() => r()));
    await host.close();
  });
  const response = await fetch(
    `http://127.0.0.1:${(server.address() as AddressInfo).port}/remedia/v1/execute`,
    { method: 'POST', body: '{}' },
  );
  expect(response.status).toBe(400);
  expect(await response.text()).toContain('before body parsers');
});
it('propagates client disconnect to an active encoded stream', async () => {
  const { host, origin } = await service();
  const cancel = vi.fn();
  const dispose = vi.fn();
  host.register({
    id: 'example',
    operations: {
      echo: () =>
        createMediaSource({
          open: () =>
            new ReadableStream<Uint8Array>({
              start(c) {
                c.enqueue(new Uint8Array([1]));
              },
              cancel,
            }),
          dispose,
        }),
    },
  });
  const grant = await host.fetch(
    new Request('http://localhost/remedia/v1/execute', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ pluginId: 'example', operation: 'echo', input: null }),
    }),
  );
  const url = (await grant.json()).result.url as string;
  await new Promise<void>((resolve, reject) => {
    const req = nodeRequest(origin + new URL(url).pathname, (res) => {
      res.once('data', () => {
        req.destroy();
        resolve();
      });
    });
    req.once('error', reject);
    req.end();
  });
  await vi.waitFor(() => expect(cancel).toHaveBeenCalledOnce());
  expect(dispose).toHaveBeenCalledOnce();
});
it('cancels a running operation when its client disconnects', async () => {
  const { host, origin } = await service();
  let started!: () => void;
  const ready = new Promise<void>((r) => {
    started = r;
  });
  const cancelled = vi.fn();
  host.register({
    id: 'example',
    operations: {
      echo: (_, { signal }) =>
        new Promise((resolve) => {
          started();
          signal.addEventListener(
            'abort',
            () => {
              cancelled();
              resolve(undefined);
            },
            { once: true },
          );
        }),
    },
  });
  const req = nodeRequest(origin + '/remedia/v1/execute', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
  });
  req.on('error', () => {});
  req.end(JSON.stringify({ pluginId: 'example', operation: 'echo', input: null }));
  await ready;
  req.destroy();
  await vi.waitFor(() => expect(cancelled).toHaveBeenCalledOnce());
});
