import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const BOT_ID = '100000000000000001';
const GUILDS = Array.from({ length: 5 }, (_, index) => ({
  guildId: String(200000000000000001n + BigInt(index)),
  channelId: String(300000000000000001n + BigInt(index)),
}));

function gatewayResponder(
  receive: (packet: unknown) => void,
  requests: DiscordGatewayVoicePayload[],
) {
  let sequence = 0;
  return (_guildId: string, payload: DiscordGatewayVoicePayload) => {
    requests.push(payload);
    if (payload.d.channel_id === null) return;
    const { guild_id: guildId, channel_id: channelId } = payload.d;
    // Dispatch on a later microtask, after the application's send callback
    // returns and the session has armed its credential collection.
    queueMicrotask(() => {
      receive({
        op: 0,
        t: 'VOICE_STATE_UPDATE',
        s: ++sequence,
        d: {
          guild_id: guildId,
          channel_id: channelId,
          user_id: BOT_ID,
          session_id: `voice-session-${sequence}`,
        },
      });
      receive({
        op: 0,
        t: 'VOICE_SERVER_UPDATE',
        s: ++sequence,
        d: {
          guild_id: guildId,
          token: `voice-token-${sequence}`,
          endpoint: 'voice.example.discord.media:443',
        },
      });
    });
  };
}

describe('concurrent Discord voice joins', () => {
  it('connects five guilds concurrently even when voice handshake exceeds the ordinary RPC deadline', async () => {
    const server = await FakeServerNode.listen({ id: 'concurrent-voice-joins' });
    server.voiceConnectGate = () => new Promise((resolve) => setTimeout(resolve, 150));
    const sent: DiscordGatewayVoicePayload[] = [];
    let receive: (packet: unknown) => void = () => undefined;
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          id: 'remote-concurrent',
          type: 'remote',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      requestTimeoutMs: 100,
      send: gatewayResponder((packet) => receive(packet), sent),
    });
    receive = (packet) => client.receive(packet);
    try {
      const sessions = GUILDS.map(({ guildId, channelId }) =>
        client.createDiscordSession({ guildId, channelId, userId: BOT_ID }),
      );
      await Promise.all(sessions.map((session) => session.connect({ timeoutMs: 3_000 })));
      expect(server.voiceConnectRequests).toBe(5);
      expect(sessions.every((session) => session.connectionState === 'connected')).toBe(true);
      expect(sent.filter((payload) => payload.d.channel_id !== null)).toHaveLength(5);
      expect(sent.some((payload) => payload.d.channel_id === null)).toBe(false);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('retries a failed initial voice transport with fresh gateway credentials', async () => {
    const server = await FakeServerNode.listen({ id: 'first-join-retry' });
    server.voiceConnectFailures = 1;
    const sent: DiscordGatewayVoicePayload[] = [];
    let receive: (packet: unknown) => void = () => undefined;
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          id: 'remote-first-join',
          type: 'remote',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: gatewayResponder((packet) => receive(packet), sent),
    });
    receive = (packet) => client.receive(packet);
    try {
      const session = client.createDiscordSession({
        ...GUILDS[0]!,
        userId: BOT_ID,
      });
      await session.connect({ timeoutMs: 3_000 });
      expect(session.connectionState).toBe('connected');
      expect(server.voiceConnectRequests).toBe(2);
      expect(server.voiceConnectionCredentials[0]?.sessionId).not.toBe(
        server.voiceConnectionCredentials[1]?.sessionId,
      );
      expect(sent.filter((payload) => payload.d.channel_id !== null)).toHaveLength(2);
      expect(sent.some((payload) => payload.d.channel_id === null)).toBe(false);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('fences a delayed first connection after its caller times out', async () => {
    const server = await FakeServerNode.listen({ id: 'first-join-timeout' });
    let release!: () => void;
    server.voiceConnectGate = () =>
      new Promise<void>((resolve) => {
        release = resolve;
      });
    const sent: DiscordGatewayVoicePayload[] = [];
    let receive: (packet: unknown) => void = () => undefined;
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          id: 'remote-first-join-timeout',
          type: 'remote',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: gatewayResponder((packet) => receive(packet), sent),
    });
    receive = (packet) => client.receive(packet);
    try {
      const session = client.createDiscordSession({ ...GUILDS[0]!, userId: BOT_ID });
      const connecting = session.connect({ timeoutMs: 250 });
      await expect(connecting).rejects.toThrow(/timed out/);
      expect(sent.at(-1)?.d.channel_id).toBeNull();
      release();
      await new Promise((resolve) => setTimeout(resolve, 30));
      expect(session.connectionState).not.toBe('connected');
    } finally {
      release?.();
      await client.close();
      await server.close();
    }
  });
});
