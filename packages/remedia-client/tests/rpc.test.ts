import { once } from 'node:events';
import { WebSocket } from 'ws';
import { afterEach, describe, expect, it } from 'vitest';
import { RpcClient } from '../lib/rpc.js';
import { FakeServerNode } from './helpers.js';

describe('bounded WebSocket RPC', () => {
  let node: FakeServerNode | undefined;
  let socket: WebSocket | undefined;
  let rpc: RpcClient | undefined;

  afterEach(async () => {
    rpc?.fail(new Error('test complete'));
    socket?.close();
    if (node) await node.close();
    node = undefined;
    socket = undefined;
    rpc = undefined;
  });

  it('correlates concurrent out-of-order responses', async () => {
    node = await FakeServerNode.listen({ id: 'rpc' });
    socket = connect(node.url);
    await once(socket, 'open');
    rpc = makeRpc(socket);

    const slow = rpc.request<{ value: string }>('test.delay', { delayMs: 80, value: 'slow' });
    const fast = rpc.request<{ value: string }>('test.delay', { delayMs: 1, value: 'fast' });
    await expect(fast).resolves.toEqual({ value: 'fast' });
    await expect(slow).resolves.toEqual({ value: 'slow' });
    expect(rpc.pendingCount).toBe(0);
  });

  it('cleans timed out and aborted requests and ignores their late replies', async () => {
    node = await FakeServerNode.listen({ id: 'rpc-cancel' });
    socket = connect(node.url);
    await once(socket, 'open');
    rpc = makeRpc(socket);

    await expect(
      rpc.request('test.delay', { delayMs: 80 }, { timeoutMs: 25 }),
    ).rejects.toMatchObject({
      code: 'request_timeout',
    });
    expect(rpc.pendingCount).toBe(0);

    const controller = new AbortController();
    const aborted = rpc.request('test.delay', { delayMs: 80 }, { signal: controller.signal });
    controller.abort();
    await expect(aborted).rejects.toMatchObject({ name: 'AbortError' });
    expect(rpc.pendingCount).toBe(0);

    await new Promise((resolve) => setTimeout(resolve, 100));

    // Recovery uses the normal request budget, even when its response takes
    // longer than the intentionally short timeout exercised above.
    const recovered = rpc.request('test.delay', { delayMs: 50, value: 'still alive' });
    await expect(recovered).resolves.toEqual({ value: 'still alive' });
    expect(rpc.pendingCount).toBe(0);
  });

  it('rejects outstanding work when its socket closes', async () => {
    node = await FakeServerNode.listen({ id: 'rpc-close' });
    socket = connect(node.url);
    await once(socket, 'open');
    rpc = makeRpc(socket);
    const pending = rpc.request('test.delay', { delayMs: 500 });
    socket.close(1011, 'test reset');
    await expect(pending).rejects.toMatchObject({ name: 'NodeUnavailableError' });
  });
});

function makeRpc(socket: WebSocket, timeoutMs = 1_000): RpcClient {
  return new RpcClient(socket, {
    nodeId: 'test-node',
    timeoutMs,
    maxPending: 4,
    onEvent: () => undefined,
    onMalformed: () => undefined,
  });
}

function connect(url: string): WebSocket {
  return new WebSocket(url, { headers: { 'x-remedia-bot-id': '100000000000000001' } });
}
