import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import { RemoteNodeAdapter } from '../lib/remote-node.js';
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const BOT_ID = '100000000000000001';
const GUILD_ID = '200000000000000001';
const CHANNEL_ID = '300000000000000001';

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });

  return { promise, resolve };
}

async function adapter(server: FakeServerNode, requestTimeoutMs = 2_000) {
  const node = new RemoteNodeAdapter({
    botUserId: BOT_ID,
    config: {
      type: 'remote',
      id: server.id,
      url: server.url,
      auth: { username: 'bot', password: 'secret' },
    },
    requestTimeoutMs,
    maxPendingRequests: 4,
  });
  await node.start();
  await node.createGuild(GUILD_ID);
  return node;
}

describe('cancelling a seek on a remote node', () => {
  it('sends the guild-addressed cancel when the caller aborts', async () => {
    const server = await FakeServerNode.listen({ id: 'seek-abort' });
    const release = deferred();
    server.seekGate = () => release.promise;
    const node = await adapter(server);
    try {
      const controller = new AbortController();
      const seeking = node.request(
        'player.seek',
        { guildId: GUILD_ID, positionMs: 500 },
        { signal: controller.signal },
      );
      const rejected = expect(seeking).rejects.toThrow();
      controller.abort();
      await rejected;

      await expect.poll(() => server.seekCancels.length).toBe(1);
      expect(server.seekCancels[0]).toEqual({ guildId: GUILD_ID });
    } finally {
      release.resolve();
      await node.close();
      await server.close();
    }
  });

  it('sends it when the request deadline passes, so the seek cannot apply later', async () => {
    const server = await FakeServerNode.listen({ id: 'seek-deadline' });
    const release = deferred();
    server.seekGate = () => release.promise;
    const node = await adapter(server, 150);
    try {
      await expect(
        node.request('player.seek', { guildId: GUILD_ID, positionMs: 500 }),
      ).rejects.toMatchObject({ code: 'request_timeout' });

      await expect.poll(() => server.seekCancels.length).toBe(1);
      expect(server.seekCancels[0]).toEqual({ guildId: GUILD_ID });
    } finally {
      release.resolve();
      await node.close();
      await server.close();
    }
  });
});

describe('the position a fallback seek reports', () => {
  it('is the position the audio continues from, and the clock follows it', async () => {
    const server = await FakeServerNode.listen({ id: 'seek-applied' });
    const sent: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guild, payload) => {
        sent.push(payload);
      },
    });
    try {
      const session = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const join = session.connect();
      await expect.poll(() => sent.at(-1)?.d.channel_id).toBe(CHANNEL_ID);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: { guild_id: GUILD_ID, channel_id: CHANNEL_ID, user_id: BOT_ID, session_id: 's' },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 't', endpoint: 'voice.example.test:443' },
      });
      await join;
      await session.play({ url: 'https://media.example.test/song.mp3' });

      const seeked: Array<Record<string, unknown>> = [];
      session.on('playbackSeeked', (event) => seeked.push(event as never));
      // The node resumes 3 s after the 90 s that was asked for.
      server.fallbackSeekLandsAfterMs = 3_000;
      await session.seek(90_000);

      expect(seeked).toHaveLength(1);
      expect(seeked[0]).toMatchObject({
        positionMs: 93_000,
        requestedPositionMs: 90_000,
        fallback: true,
      });
      expect((await session.refresh()).position.mediaMs).toBe(93_000);
      expect(session.getMediaTime()).toBeGreaterThanOrEqual(93_000);
      expect(session.getMediaTime()).toBeLessThan(94_000);
    } finally {
      await client.close();
      await server.close();
    }
  });
});
