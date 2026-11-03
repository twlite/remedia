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
    socket = new WebSocket(node.url);
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
    socket = new WebSocket(node.url);
    await once(socket, 'open');
    rpc = makeRpc(socket, 25);

    await expect(rpc.request('test.delay', { delayMs: 80 })).rejects.toMatchObject({
      code: 'request_timeout',
    });
    const controller = new AbortController();
    const aborted = rpc.request('test.delay', { delayMs: 80 }, { signal: controller.signal });
    controller.abort();
    await expect(aborted).rejects.toMatchObject({ name: 'AbortError' });
    await new Promise((resolve) => setTimeout(resolve, 100));
    await expect(rpc.request('test.delay', { delayMs: 0, value: 'still alive' })).resolves.toEqual({
      value: 'still alive',
    });
    expect(rpc.pendingCount).toBe(0);
  });

  it('rejects outstanding work when its socket closes', async () => {
    node = await FakeServerNode.listen({ id: 'rpc-close' });
    socket = new WebSocket(node.url);
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
