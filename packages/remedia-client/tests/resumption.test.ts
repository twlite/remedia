import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import type { DiscordGatewayVoicePayload, RemediaClientEvents } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

describe('remote session resumption', () => {
  it('resumes a detached server session and retains sticky guild ownership', async () => {
    const server = await FakeServerNode.listen({ id: 'resume' });
    let resolveJoin!: () => void;
    const joinSent = new Promise<void>((resolve) => {
      resolveJoin = resolve;
    });
    const client = await RemediaClient.create({
      botUserId: server.botUserId,
      nodes: [
        {
          type: 'remote',
          id: 'remote-resume',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, _payload: DiscordGatewayVoicePayload) => {
        resolveJoin();
      },
      requestTimeoutMs: 1_500,
    });
    try {
      const session = client.createDiscordSession({
        guildId: '200000000000000002',
        channelId: '300000000000000002',
        userId: server.botUserId,
      });
      const pending = session.connect({ timeoutMs: 1_000 });
      await joinSent;
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: session.guildId,
          user_id: server.botUserId,
          channel_id: session.channelId,
          session_id: 'voice-session',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: {
          guild_id: session.guildId,
          token: 'voice-token',
          endpoint: 'voice.example.discord.media:443',
        },
      });
      await pending;
      await session.play({ url: 'https://media.example.test/resume.mp3' });
      const originalSessionId = [...server.sessions.keys()][0];
      const resumed = onceEvent(client, 'sessionResumed');
      server.closeSockets();
      await resumed;
      expect(client.nodes[0]?.status).toBe('healthy');
      expect([...server.sessions.keys()]).toEqual([originalSessionId]);
      expect(client.getAssignment(session.guildId)).toMatchObject({
        nodeId: 'remote-resume',
        status: 'assigned',
      });
      expect((await session.refresh()).generation).toBe(1);
      await session.dispose();
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('reports an expired session instead of silently assigning its guild elsewhere', async () => {
    const serverA = await FakeServerNode.listen({ id: 'expired', dropSessionsOnDisconnect: true });
    const serverB = await FakeServerNode.listen({ id: 'replacement' });
    let resolveJoin!: () => void;
    const joinSent = new Promise<void>((resolve) => {
      resolveJoin = resolve;
    });
    const client = await RemediaClient.create({
      botUserId: serverA.botUserId,
      nodes: [
        {
          type: 'remote',
          id: 'node-a',
          url: serverA.url,
          auth: { username: 'bot', password: 'secret' },
          capacity: 10,
        },
        {
          type: 'remote',
          id: 'node-b',
          url: serverB.url,
          auth: { username: 'bot', password: 'secret' },
          capacity: 10,
        },
      ],
      send: (_guildId, _payload: DiscordGatewayVoicePayload) => {
        resolveJoin();
      },
      requestTimeoutMs: 800,
    });
    try {
      const session = client.createDiscordSession({
        guildId: '200000000000000003',
        channelId: '300000000000000003',
        userId: serverA.botUserId,
        nodeId: 'node-a',
      });
      const pending = session.connect({ timeoutMs: 800 });
      await joinSent;
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: session.guildId,
          user_id: serverA.botUserId,
          channel_id: session.channelId,
          session_id: 'voice-session',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: {
          guild_id: session.guildId,
          token: 'voice-token',
          endpoint: 'voice.example.discord.media:443',
        },
      });
      await pending;
      const lost = onceEvent(client, 'sessionLost');
      serverA.closeSockets();
      await lost;
      expect(client.getAssignment(session.guildId)?.status).toBe('lost');
      await expect(
        Promise.resolve().then(() => session.play({ url: 'https://media.example.test/lost.mp3' })),
      ).rejects.toThrow(/lost|expired/i);
      await session.dispose();
    } finally {
      await client.close();
      await serverA.close();
      await serverB.close();
    }
  });
});

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
