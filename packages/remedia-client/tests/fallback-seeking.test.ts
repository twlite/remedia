import { describe, expect, it } from 'vitest';
import { RemoteNodeAdapter } from '../lib/remote-node.js';
import { FakeServerNode, type FakeServerOptions } from './helpers.js';

const BOT_ID = '100000000000000001';

async function withNode<T>(
  options: Partial<FakeServerOptions>,
  run: (node: RemoteNodeAdapter, server: FakeServerNode) => Promise<T>,
): Promise<T> {
  const server = await FakeServerNode.listen({ id: 'seek-node', ...options });
  const node = new RemoteNodeAdapter({
    botUserId: BOT_ID,
    config: {
      type: 'remote',
      id: 'seek-node',
      url: server.url,
      auth: { username: 'bot', password: 'secret' },
    },
    requestTimeoutMs: 2_000,
    maxPendingRequests: 4,
  });

  try {
    await node.start();
    return await run(node, server);
  } finally {
    await node.close();
    await server.close();
  }
}

describe('fallback seeking capability', () => {
  it('reports a node that advertises it as available', async () => {
    await withNode({ fallbackSeekingCapability: { available: true } }, async (node) => {
      expect(node.snapshot.capabilities?.fallbackSeeking).toEqual({ available: true });
    });
  });

  it('treats a node that predates it as unavailable', async () => {
    await withNode({}, async (node) => {
      expect(node.snapshot.capabilities?.fallbackSeeking).toEqual({ available: false });
    });
  });

  it.each([{ available: 'yes' }, { available: 1 }, {}, null])(
    'fails closed on a malformed advertisement %j',
    async (advertised) => {
      await withNode(
        { fallbackSeekingCapability: advertised as unknown as Record<string, unknown> },
        async (node) => {
          expect(node.snapshot.capabilities?.fallbackSeeking).toEqual({ available: false });
        },
      );
    },
  );
});

describe('creating a guild with fallback seeking', () => {
  it('sends the opt-in only when it was requested', async () => {
    await withNode({ fallbackSeekingCapability: { available: true } }, async (node, server) => {
      await node.createGuild('200000000000000001');
      await node.createGuild('200000000000000002', undefined, false);
      await node.createGuild('200000000000000003', undefined, true);

      expect(server.guildCreateParams.map((params) => params.enableFallbackSeeking)).toEqual([
        undefined,
        undefined,
        true,
      ]);
    });
  });
});
