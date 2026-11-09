import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Readable } from 'node:stream';
import { createPluginHost } from '../index.js';

const origin = 'http://localhost:3000';
const secret = 'test-token-123456';

function create() {
  const host = createPluginHost({ name: 'Test plugins', publicUrl: origin, token: secret });
  host.register({
    id: 'example.test',
    operations: {
      'search.query.resolve': ({ query }) => ({ tracks: [{ title: query }] }),
      'track.stream': () => Readable.from([Buffer.from('encoded-data')]),
    },
  });
  return host;
}

function execute(host, operation, input, auth = secret) {
  return host.fetch(new Request(origin + '/remedia/v1/execute', {
    method: 'POST',
    headers: { authorization: 'Bearer ' + auth, 'content-type': 'application/json' },
    body: JSON.stringify({ pluginId: 'example.test', operation, input }),
  }));
}

test('discovery lists registered plugins and operations', async () => {
  const response = await create().fetch(new Request(origin + '/.well-known/remedia'));
  assert.equal(response.status, 200);
  const value = await response.json();
  assert.equal(value.protocolVersion, 1);
  assert.deepEqual(value.plugins[0].operations, ['search.query.resolve', 'track.stream']);
});

test('execute authenticates and invokes an operation', async () => {
  const host = create();
  assert.equal((await execute(host, 'search.query.resolve', { query: 'song' }, 'invalid')).status, 401);
  const response = await execute(host, 'search.query.resolve', { query: 'song' });
  assert.deepEqual((await response.json()).result, { tracks: [{ title: 'song' }] });
});

test('one-use media URL streams encoded bytes', async () => {
  const host = create();
  const response = await execute(host, 'track.stream', {});
  const descriptor = (await response.json()).result;
  assert.match(descriptor.url, /\/remedia\/v1\/media\//);
  const media = await host.fetch(new Request(descriptor.url));
  assert.equal(media.status, 200);
  assert.equal(await media.text(), 'encoded-data');
  assert.equal((await host.fetch(new Request(descriptor.url))).status, 404);
});

test('malformed, missing and unknown operations fail', async () => {
  const host = create();
  assert.equal((await execute(host, 'missing', {})).status, 404);
  const invalid = await host.fetch(new Request(origin + '/remedia/v1/execute', {
    method: 'POST', headers: { authorization: 'Bearer ' + secret }, body: '{invalid',
  }));
  assert.equal(invalid.status, 400);
});

test('unregistration updates discovery', async () => {
  const host = create();
  assert.equal(await host.unregister('example.test'), true);
  const response = await host.fetch(new Request(origin + '/.well-known/remedia'));
  assert.deepEqual((await response.json()).plugins, []);
});
