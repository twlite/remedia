import { createServer, type IncomingMessage } from 'node:http';
import type { Socket } from 'node:net';
import { describe, expect, it } from 'vitest';
import type { ServerLifecycleEvent } from '../lib/types.js';
import { RemoteNodeAdapter } from '../lib/remote-node.js';
import { FakeServerNode } from './helpers.js';

describe('remote resource cleanup', () => {
  it('flushes guild destruction after resuming a temporarily disconnected node', async () => {
    const server = await FakeServerNode.listen({ id: 'cleanup-resume' });
    const node = new RemoteNodeAdapter({
      botUserId: server.botUserId,
      config: {
        type: 'remote',
        id: 'cleanup-node',
        url: server.url,
        auth: { username: 'bot', password: 'secret' },
      },
      requestTimeoutMs: 1_500,
      maxPendingRequests: 16,
    });
    try {
      await node.start();
      const guildId = '200000000000000401';
      await node.createGuild(guildId);

      const disconnected = waitForStatus(node, 'disconnected');
      server.closeSockets();
      await disconnected;

      const destroyed = waitForLifecycle(node, 'guild.destroyed');
      await node.destroyGuild(guildId);
      expect(node.snapshot.assignedGuilds).not.toContain(guildId);

      await destroyed;
      expect(node.status).toBe('healthy');
      expect(node.snapshot.assignedGuilds).not.toContain(guildId);
      await node.createGuild(guildId);
      expect(node.snapshot.assignedGuilds).toContain(guildId);
    } finally {
      await node.close();
      await server.close();
    }
  }, 10_000);

  it('cancels a WebSocket handshake when the client closes during startup', async () => {
    const server = createServer();
    let upgradedSocket: Socket | undefined;
    let resolveUpgrade!: () => void;
    const upgradeStarted = new Promise<void>((resolve) => {
      resolveUpgrade = resolve;
    });
    server.on('upgrade', (_request: IncomingMessage, socket: Socket) => {
      upgradedSocket = socket;
      resolveUpgrade();
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('test server did not bind');
    const node = new RemoteNodeAdapter({
      botUserId: '100000000000000001',
      config: {
        type: 'remote',
        id: 'closing-node',
        url: `ws://127.0.0.1:${address.port}/v1/ws`,
        auth: { username: 'bot', password: 'secret' },
      },
      requestTimeoutMs: 10_000,
      maxPendingRequests: 16,
    });
    try {
      const starting = node.start();
      await upgradeStarted;
      await node.close();
      await starting;
      expect(node.status).toBe('closed');
    } finally {
      await node.close();
      upgradedSocket?.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  }, 5_000);
});

function waitForStatus(node: RemoteNodeAdapter, status: 'healthy' | 'disconnected'): Promise<void> {
  if (node.status === status) return Promise.resolve();
  return new Promise((resolve) => {
    const listener = (event: { snapshot: { status: string } }): void => {
      if (event.snapshot.status === status) {
        node.off('status', listener);
        resolve();
      }
    };
    node.on('status', listener);
  });
}

function waitForLifecycle(node: RemoteNodeAdapter, name: string): Promise<ServerLifecycleEvent> {
  return new Promise((resolve) => {
    const listener = (event: ServerLifecycleEvent): void => {
      if (event.event === name) {
        node.off('lifecycle', listener);
        resolve(event);
      }
    };
    node.on('lifecycle', listener);
  });
}
