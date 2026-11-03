import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import type { DiscordSessionOptions, RemediaClientEvents } from '../lib/types.js';
import type { GuildBackend } from '../lib/adapters.js';
import { FakeServerNode } from './helpers.js';

describe('multi-node assignment', () => {
  it('distributes new guilds, keeps existing affinity, and isolates a failed node', async () => {
    const serverA = await FakeServerNode.listen({ id: 'pool-a', playersMaximum: 4 });
    const serverB = await FakeServerNode.listen({ id: 'pool-b', playersMaximum: 4 });
    const client = await RemediaClient.create({
      botUserId: serverA.botUserId,
      nodes: [
        {
          type: 'remote',
          id: 'node-a',
          url: serverA.url,
          auth: { username: 'bot', password: 'secret' },
        },
        {
          type: 'remote',
          id: 'node-b',
          url: serverB.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: () => undefined,
    });
    const allocated: Array<{
      session: ReturnType<RemediaClient['createDiscordSession']>;
      backend: GuildBackend;
    }> = [];
    try {
      const runtime = client as unknown as {
        resolveBackend(
          options: DiscordSessionOptions,
          onDispose: () => void,
        ): Promise<GuildBackend>;
        releaseSession(session: ReturnType<RemediaClient['createDiscordSession']>): void;
      };
      const firstOptions = [1, 2, 3, 4].map(optionsFor);
      const firstSessions = firstOptions.map((options) => client.createDiscordSession(options));
      const firstBackends = await Promise.all(
        firstOptions.map((options, index) =>
          runtime.resolveBackend(options, () => runtime.releaseSession(firstSessions[index]!)),
        ),
      );
      allocated.push(
        ...firstSessions.map((session, index) => ({ session, backend: firstBackends[index]! })),
      );
      expect(allocated.map(({ backend }) => backend.nodeId)).toEqual([
        'node-a',
        'node-b',
        'node-a',
        'node-b',
      ]);

      const disconnected = onceEvent(client, 'nodeDisconnected');
      await serverA.close();
      await expect(disconnected).resolves.toMatchObject({
        node: { id: 'node-a', status: 'disconnected' },
      });
      expect(client.getAssignment(allocated[0]!.session.guildId)).toMatchObject({
        nodeId: 'node-a',
        status: 'assigned',
      });
      expect(client.nodes.find((node) => node.id === 'node-b')?.status).toBe('healthy');

      const nextOptions = optionsFor(5);
      const nextSession = client.createDiscordSession(nextOptions);
      const nextBackend = await runtime.resolveBackend(nextOptions, () =>
        runtime.releaseSession(nextSession),
      );
      allocated.push({ session: nextSession, backend: nextBackend });
      expect(nextBackend.nodeId).toBe('node-b');
      expect(client.getAssignment(allocated[0]!.session.guildId)?.nodeId).toBe('node-a');
    } finally {
      await Promise.all(
        allocated.map(async ({ backend, session }) => {
          await backend.dispose().catch(() => undefined);
          await session.dispose().catch(() => undefined);
        }),
      );
      await client.close();
      await serverB.close();
    }
  });
});

function optionsFor(index: number): DiscordSessionOptions {
  const suffix = String(index).padStart(2, '0');
  return {
    guildId: `2000000000000000${suffix}`,
    channelId: `3000000000000000${suffix}`,
    userId: '100000000000000001',
  };
}

function onceEvent<Event extends keyof RemediaClientEvents>(
  client: RemediaClient,
  event: Event,
): Promise<RemediaClientEvents[Event][0]> {
  return new Promise((resolve) => {
    const listener = (...args: RemediaClientEvents[Event]): void => {
      client.off(event, listener as never);
      resolve(args[0]!);
    };
    client.on(event, listener as never);
  });
}
