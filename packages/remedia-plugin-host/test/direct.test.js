import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Readable } from 'node:stream';
import { createMediaSource, createPluginHost, isMediaSource } from '../index.js';

test('in-process execution preserves identity and never needs publicUrl', async () => {
  const host = createPluginHost({ name: 'Direct' });
  const value = new Map([['key', 42]]);
  host.register({ id: 'org.example.direct', operations: { get: () => value } });
  const result = await host.execute('org.example.direct', 'get', {});
  assert.equal(result.handled, true);
  assert.strictEqual(result.result, value);
  assert.equal((await host.execute('org.example.direct', 'get', {})).result.get('key'), 42);
  await host.close();
});

test('undefined means unhandled, including direct calls', async () => {
  const host = createPluginHost({ name: 'Direct' });
  host.register({ id: 'org.example.direct', operations: { decline: () => undefined, negative: () => false } });
  assert.deepEqual(await host.execute('org.example.direct', 'decline', {}), { handled: false });
  assert.deepEqual(await host.execute('org.example.direct', 'negative', {}), { handled: true, result: false });
  await host.close();
});

test('media factory opens lazily only after URL redemption', async () => {
  const host = createPluginHost({ name: 'Lazy', publicUrl: 'http://localhost:3000' });
  let opened = 0;
  const media = createMediaSource({ open: () => { opened++; return Readable.from(['media']); }, contentType: 'audio/webm' });
  assert.equal(isMediaSource(media), true);
  host.register({ id: 'org.example.lazy', operations: { anyName: () => media } });
  assert.strictEqual((await host.execute('org.example.lazy', 'anyName', {})).result, media);
  const response = await host.fetch(new Request('http://localhost:3000/remedia/v1/execute', {
    method: 'POST',
    body: JSON.stringify({ pluginId: 'org.example.lazy', operation: 'anyName', input: {} }),
  }));
  assert.equal(opened, 0);
  const descriptor = (await response.json()).result;
  const streamed = await host.fetch(new Request(descriptor.url));
  assert.equal(streamed.headers.get('content-type'), 'audio/webm');
  assert.equal(await streamed.text(), 'media');
  assert.equal(opened, 1);
  await host.close();
});

test('unregister aborts a handler and disposes the plugin', async () => {
  const host = createPluginHost({ name: 'Direct' });
  let disposed = false;
  let observed;
  host.register({
    id: 'org.example.cancel',
    operations: { active: (_, ctx) => { observed = ctx.signal; return new Promise((resolve) => {
      ctx.signal.addEventListener('abort', () => resolve(undefined), { once: true });
    }); } },
    dispose: () => { disposed = true; },
  });
  const pending = host.execute('org.example.cancel', 'active', {}).catch(() => undefined);
  await new Promise((resolve) => setImmediate(resolve));
  await host.unregister('org.example.cancel');
  await pending;
  assert.equal(observed.aborted, true);
  assert.equal(disposed, true);
  await host.close();
});
