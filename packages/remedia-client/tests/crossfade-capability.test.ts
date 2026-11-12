import { describe, expect, it } from 'vitest';
import { RemoteNodeAdapter } from '../lib/remote-node.js';
import { FakeServerNode } from './helpers.js';

const BOT_ID = '100000000000000001';

async function capabilitiesAdvertisedAs(crossfadeCapability: Record<string, unknown>) {
  const server = await FakeServerNode.listen({ id: 'curves', crossfadeCapability });
  const node = new RemoteNodeAdapter({
    botUserId: BOT_ID,
    config: {
      type: 'remote',
      id: 'curves',
      url: server.url,
      auth: { username: 'bot', password: 'secret' },
    },
    requestTimeoutMs: 2_000,
    maxPendingRequests: 4,
  });

  try {
    await node.start();
    return node.snapshot.capabilities?.crossfade;
  } finally {
    await node.close();
    await server.close();
  }
}

const base = { configured: true, available: true, durationMs: 5000, prepareBeforeMs: 30000 };

describe('crossfade curve capability', () => {
  it('reports every curve a current node can mix with', async () => {
    expect(
      await capabilitiesAdvertisedAs({
        ...base,
        curve: 'adaptive',
        curves: ['equalPower', 'adaptive'],
      }),
    ).toEqual({ ...base, curve: 'adaptive', curves: ['equalPower', 'adaptive'] });
  });

  it('treats a node that predates the adaptive curve as equalPower only', async () => {
    expect(await capabilitiesAdvertisedAs({ ...base, curve: 'equalPower' })).toEqual({
      ...base,
      curve: 'equalPower',
      curves: ['equalPower'],
    });
  });

  it('fails closed on a default curve the client does not know', async () => {
    const reported = await capabilitiesAdvertisedAs({
      ...base,
      curve: 'linear',
      curves: ['equalPower', 'linear'],
    });

    expect(reported?.available).toBe(false);
    expect(reported?.curve).toBe('equalPower');
    expect(reported?.curves).toEqual(['equalPower']);
  });
});
