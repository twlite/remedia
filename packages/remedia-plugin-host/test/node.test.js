import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { test } from 'node:test';
import { createPluginHost } from '../index.js';
import { createNodeMiddleware } from '../node.js';

test('Node middleware handles Fetch requests and responses', async () => {
  const host = createPluginHost({ name: 'Node test', publicUrl: 'http://127.0.0.1:3000' });
  host.register({ id: 'example.echo', operations: { echo: ({ value }) => ({ value }) } });
  const middleware = createNodeMiddleware(host);
  const server = createServer((req, res) => void middleware(req, res));
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  try {
    const base = 'http://127.0.0.1:' + server.address().port;
    const manifest = await fetch(base + '/.well-known/remedia');
    assert.equal(manifest.status, 200);
    const response = await fetch(base + '/remedia/v1/execute', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ pluginId: 'example.echo', operation: 'echo', input: { value: 42 } }),
    });
    assert.equal(response.status, 200);
    assert.deepEqual((await response.json()).result, { value: 42 });
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});
