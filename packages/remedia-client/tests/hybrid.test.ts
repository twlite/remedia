import { EventEmitter } from 'node:events';
import { describe, expect, it } from 'vitest';
import { PlaybackSessionEvents, RemediaClient } from '../lib/index.js';
import type { DiscordSessionOptions, RemediaClientEvents } from '../lib/types.js';
import type { GuildBackend } from '../lib/adapters.js';
import { FakeServerNode } from './helpers.js';

describe('hybrid embedded and remote node pool', () => {
  it('distributes sticky guild assignments across one embedded and two remote nodes', async () => {
    const serverA = await FakeServerNode.listen({ id: 'hybrid-a', playersMaximum: 2 });
    const serverB = await FakeServerNode.listen({ id: 'hybrid-b', playersMaximum: 2 });
    const client = await RemediaClient.create({
      botUserId: serverA.botUserId,
      nodes: [
        { type: 'embedded', id: 'embedded', capacity: 2 },
        {
          type: 'remote',
          id: 'remote-a',
          url: serverA.url,
          auth: { username: 'bot', password: 'secret' },
          capacity: 2,
        },
        {
          type: 'remote',
          id: 'remote-b',
          url: serverB.url,
          auth: { username: 'bot', password: 'secret' },
          capacity: 2,
        },
      ],
      send: () => undefined,
    });
    const sessions: Array<{
      session: ReturnType<RemediaClient['createDiscordSession']>;
      backend: GuildBackend;
    }> = [];
    try {
      expect(client.nodes.find((node) => node.id === 'embedded')?.status).toBe('healthy');
      const runtime = client as unknown as {
        resolveBackend(
          options: DiscordSessionOptions,
          onDispose: () => void,
        ): Promise<GuildBackend>;
        releaseSession(session: ReturnType<RemediaClient['createDiscordSession']>): void;
      };
      for (let index = 1; index <= 3; index += 1) {
        const session = client.createDiscordSession({
          guildId: `20000000000000000${index}`,
          channelId: `30000000000000000${index}`,
          userId: '100000000000000001',
        });
        const backend = await runtime.resolveBackend(
          { guildId: session.guildId, channelId: session.channelId, userId: session.userId },
          () => runtime.releaseSession(session),
        );
        sessions.push({ session, backend });
      }
      expect(sessions.map(({ backend }) => backend.nodeType)).toEqual([
        'embedded',
        'remote',
        'remote',
      ]);
      expect(new Set(sessions.map(({ backend }) => backend.nodeId)).size).toBe(3);
      expect(client.assignments.map(({ nodeId }) => nodeId)).toEqual([
        'embedded',
        'remote-a',
        'remote-b',
      ]);
      for (const { backend } of sessions) {
        expect(typeof backend.play).toBe('function');
        expect(typeof backend.pause).toBe('function');
        expect(typeof backend.resume).toBe('function');
        expect(typeof backend.setFilterGraph).toBe('function');
        expect(typeof backend.setVolume).toBe('function');
      }

      const remotePlayback = sessions[1]!.backend;
      await remotePlayback.connectWithCredentials?.({
        sessionId: 'voice-session-hybrid',
        token: 'voice-token-hybrid',
        endpoint: 'voice.example.test:443',
      });

      const embeddedFilterChange = onceBackendEvent(
        sessions[0]!.backend,
        PlaybackSessionEvents.filterChanged,
      );
      const remoteFilterChange = onceBackendEvent(
        remotePlayback,
        PlaybackSessionEvents.filterChanged,
      );
      await Promise.all([
        sessions[0]!.backend.setFilterGraph('anull'),
        remotePlayback.setFilterGraph('anull'),
      ]);
      const [embeddedFilterEvent, remoteFilterEvent] = await Promise.all([
        embeddedFilterChange,
        remoteFilterChange,
      ]);
      for (const key of [
        'sessionId',
        'guildId',
        'nodeId',
        'generation',
        'previousGraph',
        'graph',
      ]) {
        expect(embeddedFilterEvent).toHaveProperty(key);
        expect(remoteFilterEvent).toHaveProperty(key);
      }
      expect(embeddedFilterEvent).toMatchObject({
        nodeId: 'embedded',
        guildId: sessions[0]!.session.guildId,
        previousGraph: null,
        graph: 'anull',
      });
      expect(remoteFilterEvent).toMatchObject({
        nodeId: 'remote-a',
        guildId: sessions[1]!.session.guildId,
        previousGraph: null,
        graph: 'anull',
      });

      const embeddedVolumeChange = onceBackendEvent(
        sessions[0]!.backend,
        PlaybackSessionEvents.volumeChanged,
      );
      const remoteVolumeChange = onceBackendEvent(
        remotePlayback,
        PlaybackSessionEvents.volumeChanged,
      );
      await Promise.all([sessions[0]!.backend.setVolume(0.75), remotePlayback.setVolume(0.75)]);
      const [embeddedVolumeEvent, remoteVolumeEvent] = await Promise.all([
        embeddedVolumeChange,
        remoteVolumeChange,
      ]);
      for (const key of [
        'sessionId',
        'guildId',
        'nodeId',
        'generation',
        'previousVolume',
        'volume',
      ]) {
        expect(embeddedVolumeEvent).toHaveProperty(key);
        expect(remoteVolumeEvent).toHaveProperty(key);
      }
      expect(embeddedVolumeEvent).toMatchObject({
        nodeId: 'embedded',
        previousVolume: 1,
        volume: 0.75,
      });
      expect(remoteVolumeEvent).toMatchObject({
        nodeId: 'remote-a',
        previousVolume: 1,
        volume: 0.75,
      });

      await remotePlayback.play({ url: 'https://media.example.test/hybrid.mp3' });
      await remotePlayback.setFilterGraph('bass=g=8');
      await remotePlayback.setVolume(0.75);
      await remotePlayback.pause();
      await remotePlayback.resume();
      await remotePlayback.seek(1_000);
      await remotePlayback.stop();
      await expect(remotePlayback.wait()).resolves.toMatchObject({ state: 'stopped' });
      expect(serverA.allocatedGuildCount).toBe(1);

      await sessions[1]!.backend.dispose();
      await sessions[1]!.session.dispose();
      expect(client.getAssignment(sessions[1]!.session.guildId)).toBeUndefined();
      expect(client.nodes.find((node) => node.id === 'remote-a')?.assignedGuilds).toEqual([]);
      expect(serverA.allocatedGuildCount).toBe(0);

      await sessions[0]!.backend.dispose();
      await sessions[0]!.session.dispose();
      const replacement = client.createDiscordSession({
        guildId: sessions[0]!.session.guildId,
        channelId: sessions[0]!.session.channelId,
        userId: sessions[0]!.session.userId,
      });
      const replacementBackend = await runtime.resolveBackend(
        {
          guildId: replacement.guildId,
          channelId: replacement.channelId,
          userId: replacement.userId,
        },
        () => runtime.releaseSession(replacement),
      );
      expect(replacementBackend.nodeType).toBe('embedded');
      await replacementBackend.dispose();
      await replacement.dispose();

      const disconnected = onceEvent(client, 'nodeDisconnected');
      await serverA.close();
      await expect(disconnected).resolves.toMatchObject({ node: { id: 'remote-a' } });
      expect(client.nodes.find((node) => node.id === 'embedded')?.status).toBe('healthy');
      expect(client.nodes.find((node) => node.id === 'remote-b')?.status).toBe('healthy');
      await sessions[2]!.backend.connectWithCredentials?.({
        sessionId: 'voice-session-remote-b',
        token: 'voice-token-remote-b',
        endpoint: 'voice.example.test:443',
      });
      await expect(
        sessions[2]!.backend.play({ url: 'https://media.example.test/isolated.mp3' }),
      ).resolves.toMatchObject({ generation: 1 });
    } finally {
      for (const { backend, session } of sessions) {
        await backend.dispose().catch(() => undefined);
        await session.dispose().catch(() => undefined);
      }
      await client.close();
      await serverA.close();
      await serverB.close();
    }
  });
});

function onceBackendEvent(backend: GuildBackend, event: string): Promise<Record<string, unknown>> {
  return new Promise((resolve) => {
    const emitter = backend as unknown as EventEmitter;
    const listener = (payload: Record<string, unknown>): void => {
      emitter.off(event, listener);
      resolve(payload);
    };
    emitter.once(event, listener);
  });
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
