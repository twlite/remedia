import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import { RemoteNodeAdapter } from '../lib/remote-node.js';
import { NodeSelectionError, UnsupportedMediaSourceError } from '../lib/errors.js';
import type { OperationMediaSource } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const BOT_ID = '100000000000000001';
const GUILD_ID = '200000000000000001';
const CHANNEL_ID = '300000000000000001';

const source: OperationMediaSource = {
  hostId: 'source-host-1',
  pluginId: 'example.source',
  operation: 'source.stream',
  input: { id: 'opaque', nested: [1, true, null] },
};

const joins = new WeakMap<RemediaClient, Promise<void>>();

async function connect(client: RemediaClient, nodeId?: string) {
  const voice = client.createDiscordSession({
    guildId: GUILD_ID,
    channelId: CHANNEL_ID,
    userId: BOT_ID,
    ...(nodeId ? { nodeId } : {}),
  });
  const connecting = voice.connect({ timeoutMs: 2_000 });

  await joins.get(client);
  client.receive({
    op: 0,
    t: 'VOICE_STATE_UPDATE',
    s: 1,
    d: { guild_id: GUILD_ID, user_id: BOT_ID, channel_id: CHANNEL_ID, session_id: 'voice-session' },
  });
  client.receive({
    op: 0,
    t: 'VOICE_SERVER_UPDATE',
    s: 2,
    d: { guild_id: GUILD_ID, token: 'voice-token', endpoint: 'voice.example.discord.media:443' },
  });
  await connecting;

  return voice;
}

async function clientFor(servers: Array<{ id: string; server: FakeServerNode }>) {
  let joined!: () => void;
  const sent = new Promise<void>((resolve) => (joined = resolve));
  const client = await RemediaClient.create({
    botUserId: BOT_ID,
    nodes: servers.map(({ id, server }) => ({
      type: 'remote' as const,
      id,
      url: server.url,
      auth: { username: 'bot', password: 'secret' },
    })),
    send: () => joined(),
    requestTimeoutMs: 2_000,
  });

  joins.set(client, sent);

  return client;
}

describe('plugin operation media sources', () => {
  it('sends the opaque operation reference to a node which advertises the host', async () => {
    const server = await FakeServerNode.listen({ id: 'ops', pluginHosts: ['source-host-1'] });
    const client = await clientFor([{ id: 'node-ops', server }]);

    try {
      const voice = await connect(client);

      await voice.play(source);
      expect(server.playSources).toEqual([{ type: 'operation', ...source }]);
      expect(voice.nodeId).toBe('node-ops');
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('rejects operation sources on older nodes without sending or falling back', async () => {
    const older = await FakeServerNode.listen({ id: 'older' });
    const newer = await FakeServerNode.listen({ id: 'newer', pluginHosts: ['source-host-1'] });
    const client = await clientFor([
      { id: 'node-older', server: older },
      { id: 'node-newer', server: newer },
    ]);

    try {
      // The guild is assigned to the older node first; its affinity is never silently changed.
      const voice = await connect(client, 'node-older');

      expect(voice.nodeId).toBe('node-older');
      await expect(voice.play(source)).rejects.toBeInstanceOf(UnsupportedMediaSourceError);
      expect(older.playerPlayRequests).toBe(0);
      expect(newer.playerPlayRequests).toBe(0);
    } finally {
      await client.close();
      await older.close();
      await newer.close();
    }
  });

  it('rejects a host ID which the assigned node does not configure', async () => {
    const server = await FakeServerNode.listen({ id: 'other-host', pluginHosts: ['another-host'] });
    const client = await clientFor([{ id: 'node-other', server }]);

    try {
      const voice = await connect(client);

      await expect(voice.play(source)).rejects.toThrow(
        /no plugin host configured with ID source-host-1/,
      );
      expect(server.playerPlayRequests).toBe(0);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('validates the reference locally and refuses to probe single-use operation sources', async () => {
    const server = await FakeServerNode.listen({ id: 'validate', pluginHosts: ['source-host-1'] });
    const client = await clientFor([{ id: 'node-validate', server }]);

    try {
      const voice = await connect(client);

      await expect(voice.play({ ...source, pluginId: '' })).rejects.toThrow(TypeError);
      await expect(voice.play({ ...source, input: undefined as never })).rejects.toThrow(
        /input is required/,
      );
      expect(server.playerPlayRequests).toBe(0);
      await expect(client.probe(source)).rejects.toBeInstanceOf(UnsupportedMediaSourceError);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('assigns guilds only to nodes which advertise the required plugin hosts', async () => {
    const without = await FakeServerNode.listen({ id: 'without' });
    const withHost = await FakeServerNode.listen({ id: 'with', pluginHosts: ['source-host-1'] });
    const client = await clientFor([
      { id: 'node-without', server: without },
      { id: 'node-with', server: withHost },
    ]);

    try {
      // Without the requirement the first healthy node wins; with it, affinity follows the host.
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
        pluginHosts: ['source-host-1'],
      });
      const backend = await (
        client as unknown as {
          resolveBackend(options: unknown, onDispose: () => void): Promise<{ nodeId: string }>;
        }
      ).resolveBackend(
        {
          guildId: GUILD_ID,
          channelId: CHANNEL_ID,
          userId: BOT_ID,
          pluginHosts: ['source-host-1'],
        },
        () => undefined,
      );

      expect(backend.nodeId).toBe('node-with');
      expect(voice.guildId).toBe(GUILD_ID);
      await expect(
        (
          client as unknown as {
            resolveBackend(options: unknown, onDispose: () => void): Promise<unknown>;
          }
        ).resolveBackend(
          {
            guildId: '200000000000000002',
            channelId: CHANNEL_ID,
            userId: BOT_ID,
            pluginHosts: ['missing-host'],
          },
          () => undefined,
        ),
      ).rejects.toBeInstanceOf(NodeSelectionError);
      await expect(
        (
          client as unknown as {
            resolveBackend(options: unknown, onDispose: () => void): Promise<unknown>;
          }
        ).resolveBackend(
          {
            guildId: '200000000000000003',
            channelId: CHANNEL_ID,
            userId: BOT_ID,
            nodeId: 'node-without',
            pluginHosts: ['source-host-1'],
          },
          () => undefined,
        ),
      ).rejects.toBeInstanceOf(NodeSelectionError);
    } finally {
      await client.close();
      await without.close();
      await withHost.close();
    }
  });

  it('reports unavailable capabilities on adapters without the field', async () => {
    const server = await FakeServerNode.listen({ id: 'legacy' });
    const node = new RemoteNodeAdapter({
      botUserId: BOT_ID,
      config: {
        type: 'remote',
        id: 'legacy',
        url: server.url,
        auth: { username: 'bot', password: 'secret' },
      },
      requestTimeoutMs: 2_000,
      maxPendingRequests: 4,
    });

    try {
      await node.start();
      expect(node.snapshot.capabilities?.pluginOperations).toEqual({ available: false, hosts: [] });
    } finally {
      await node.close();
      await server.close();
    }
  });
});
