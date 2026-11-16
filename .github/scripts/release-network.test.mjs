import assert from 'node:assert/strict';
import test from 'node:test';
import { requestWithRetry, describeError } from './release-network.mjs';

test('retries transient API/network errors with a bounded delay', async () => {
  let calls = 0;
  const waits = [];
  const result = await requestWithRetry(
    'https://example.test/asset',
    {},
    {
      fetchImpl: async () => {
        calls++;
        if (calls === 1)
          throw new Error('fetch failed', {
            cause: Object.assign(new Error('connection reset'), { code: 'ECONNRESET' }),
          });
        if (calls === 2) return new Response('', { status: 503, headers: { 'retry-after': '90' } });
        return new Response('ok');
      },
      wait: async (ms) => {
        waits.push(ms);
      },
    },
  );
  assert.equal(await result.text(), 'ok');
  assert.equal(calls, 3);
  assert.deepEqual(waits, [1000, 30_000]);
});

test('permanent HTTP errors do not retry; terminal transport errors include the cause and operation', async () => {
  const denied = await requestWithRetry(
    'https://example.test/asset',
    {},
    {
      fetchImpl: async () => new Response('', { status: 403 }),
      wait: () => assert.fail('must not retry'),
    },
  );
  assert.equal(denied.status, 403);
  let calls = 0;
  await assert.rejects(
    requestWithRetry(
      'https://example.test/asset',
      {},
      {
        fetchImpl: async () => {
          calls++;
          throw new Error('fetch failed', { cause: new Error('socket reset') });
        },
        wait: async () => {},
      },
    ),
    /GET.*asset.*fetch failed.*socket reset/,
  );
  assert.equal(calls, 4);
  assert.match(describeError(new AggregateError([new Error('root')], 'outer')), /outer.*root/);
});
