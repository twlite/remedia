import { describe, expect, it } from 'vitest';
import { RemediaClient, RemediaClientEventNames } from '../lib/index.js';
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const guildId = '200000000000000001';
const otherGuildId = '200000000000000002';
const channelId = '300000000000000001';
const userId = '100000000000000001';

describe('cross-node Discord player migration', () => {
  async function setup(capacity = 2) {
    const source = await FakeServerNode.listen({ id: 'migration-source', playersMaximum: capacity });
    const destination = await FakeServerNode.listen({ id: 'migration-target', playersMaximum: capacity });
    const sent: DiscordGatewayVoicePayload[] = [];
    let sequence = 0;
    const client = await RemediaClient.create({
      botUserId: userId,
      nodes: [
        { type: 'remote', id: 'source', url: source.url, auth: { username: 'bot', password: 'secret' }, capacity },
        { type: 'remote', id: 'target', url: destination.url, auth: { username: 'bot', password: 'secret' }, capacity },
      ],
      send: (_guildId, packet) => { sent.push(packet); },
    });
    function credentials(id = guildId) {
      client.receive({
        t: 'VOICE_STATE_UPDATE', s: ++sequence,
        d: { guild_id: id, channel_id: channelId, user_id: userId, session_id: `session-${sequence}` },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE', s: ++sequence,
        d: { guild_id: id, token: `token-${sequence}`, endpoint: 'voice.example.test:443' },
      });
    }
    async function acknowledge(index: number, id = guildId) {
      await expect.poll(() => sent.length).toBeGreaterThanOrEqual(index);
      credentials(id);
    }
    async function cleanup() {
      await client.close();
      await Promise.all([source.close(), destination.close()]);
    }
    return { source, destination, client, sent, credentials, acknowledge, cleanup };
  }

  it('moves an idle player without replacing its public session or losing settings', async () => {
    const h = await setup();
    try {
      const session = h.client.createDiscordSession({
        guildId, channelId, userId, nodeId: 'source',
        volume: 0.6, filterGraph: 'anull', selfDeaf: false, selfMute: true,
      });
      const join = session.connect();
      await h.acknowledge(1);
      await join;

      const events: unknown[] = [];
      h.client.on(RemediaClientEventNames.guildMigrated, (event) => events.push(event));
      const migration = session.migrate('target');
      await h.acknowledge(2);
      await migration;

      expect(h.client.createDiscordSession).toBeTypeOf('function');
      expect(session.nodeId).toBe('target');
      expect(session.connectionState).toBe('connected');
      expect(session.volume).toBe(0.6);
      expect(session.filters.graph).toBe('anull');
      expect(session.selfDeaf).toBe(false);
      expect(session.selfMute).toBe(true);
      expect(h.client.assignments).toEqual([
        { guildId, nodeId: 'target', nodeType: 'remote', status: 'assigned' },
      ]);
      expect(events).toEqual([{ guildId, fromNodeId: 'source', toNodeId: 'target', nodeType: 'remote' }]);
      expect(h.source.allocatedGuildCount).toBe(0);
      expect(h.destination.allocatedGuildCount).toBe(1);
      await session.dispose();
      expect(h.client.assignments).toEqual([]);
      expect(h.destination.allocatedGuildCount).toBe(0);
    } finally {
      await h.cleanup();
    }
  });

  it('resumes paused replayable URL playback on the destination near its source position', async () => {
    const h = await setup();
    try {
      const session = h.client.createDiscordSession({ guildId, channelId, userId, nodeId: 'source' });
      const join = session.connect();
      await h.acknowledge(1);
      await join;
      const customMetadata = { trackId: 'song-42' };
      await session.play({ url: 'https://media.example.test/song.mp3' }, { customMetadata });
      await session.seek(850);
      await session.pause();

      const started: Array<{ customMetadata?: unknown }> = [];
      const stopped: unknown[] = [];
      session.on('playbackStarted', (event) => started.push(event));
      session.on('playbackStopped', (event) => stopped.push(event));
      const migration = session.migrate('target');
      await h.acknowledge(2);
      await migration;

      expect(h.source.playerPlayRequests).toBe(1);
      expect(h.destination.playerPlayRequests).toBe(1);
      expect(session.nodeId).toBe('target');
      expect(session.state).toBe('paused');
      expect(session.position.mediaMs).toBe(850);
      expect(started).toHaveLength(1);
      expect(started[0]?.customMetadata).toEqual(customMetadata);
      expect(stopped).toEqual([]);
      await session.dispose();
    } finally {
      await h.cleanup();
    }
  });

  it('rejects active migration without a usable position before changing nodes', async () => {
    const h = await setup();
    try {
      const session = h.client.createDiscordSession({ guildId, channelId, userId, nodeId: 'source' });
      const join = session.connect();
      await h.acknowledge(1);
      await join;
      await session.play({ url: 'https://media.example.test/song.mp3' });
      await expect(session.migrate('target')).rejects.toThrow(/known media position/);
      expect(session.nodeId).toBe('source');
      expect(session.state).toBe('playing');
      expect(h.destination.allocatedGuildCount).toBe(0);
    } finally {
      await h.cleanup();
    }
  });

  it('rejects migration to a full node without disrupting the original player', async () => {
    const h = await setup(1);
    try {
      const first = h.client.createDiscordSession({ guildId, channelId, userId, nodeId: 'source' });
      const second = h.client.createDiscordSession({
        guildId: otherGuildId, channelId, userId, nodeId: 'target',
      });
      const one = first.connect();
      await h.acknowledge(1);
      await one;
      const two = second.connect();
      await h.acknowledge(2, otherGuildId);
      await two;
      await expect(first.migrate('target')).rejects.toThrow(/capacity|unhealthy/);
      expect(first.nodeId).toBe('source');
      expect(first.connectionState).toBe('connected');
      expect(h.source.allocatedGuildCount).toBe(1);
      expect(h.destination.allocatedGuildCount).toBe(1);
    } finally {
      await h.cleanup();
    }
  });
});
