import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const GUILD_ID = '200000000000000001';
const CHANNEL_ID = '300000000000000001';
const BOT_ID = '100000000000000001';

describe('remote Discord playback', () => {
  it('connects through host Gateway events and controls playback by generation', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-playback' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    let resolveJoin!: () => void;
    const joinSent = new Promise<void>((resolve) => {
      resolveJoin = resolve;
    });
    const client = await RemediaClient.create({
      nodes: [
        {
          type: 'remote',
          id: 'remote-a',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
        resolveJoin();
      },
      requestTimeoutMs: 2_000,
    });
    try {
      expect(server.authorizationHeaders[0]).toBe(
        `Basic ${Buffer.from('bot:secret').toString('base64')}`,
      );
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await joinSent;
      expect(payloads[0]?.d).toEqual({
        guild_id: GUILD_ID,
        channel_id: CHANNEL_ID,
        self_mute: false,
        self_deaf: false,
      });
      client.receive({
        op: 0,
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session',
        },
      });
      client.receive({
        op: 0,
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: {
          guild_id: GUILD_ID,
          token: 'voice-token',
          endpoint: 'voice.example.discord.media:443',
        },
      });
      await connecting;
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 3,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session',
        },
      });
      expect(payloads).toHaveLength(1);

      expect(voice.nodeType).toBe('remote');
      expect(voice.nodeId).toBe('remote-a');
      await expect(
        voice.play({ url: 'https://media.example.test/song.mp3' }),
      ).resolves.toMatchObject({ codec: 'mp3' });
      await voice.filters.set('bass=g=8');
      await voice.setVolume(0.75);
      await voice.pause();
      expect(voice.state).toBe('paused');
      await voice.resume();
      await voice.seek(1_500);
      expect((await voice.refresh()).position.mediaMs).toBe(1_500);

      await voice.stop();
      server.finishBeforePlayResponse = true;
      await voice.play({ file: '/srv/media/next.ogg' });
      expect(voice.state).toBe('completed');
      await expect(voice.wait()).resolves.toBe('completed');
      expect(voice.volume).toBe(0.75);
      await expect(voice.play({ buffer: Buffer.from('not remote') })).rejects.toThrow(
        /remote Remedia nodes accept only/,
      );

      await voice.dispose();
      expect(payloads.at(-1)?.d.channel_id).toBeNull();
      expect(client.assignments).toEqual([]);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('keeps an authentication failure visible without retrying bad credentials', async () => {
    const server = await FakeServerNode.listen({
      id: 'remote-auth',
      requiredAuthorization: `Basic ${Buffer.from('bot:correct').toString('base64')}`,
    });
    const client = await RemediaClient.create({
      nodes: [
        {
          type: 'remote',
          id: 'remote-auth',
          url: server.url,
          auth: { username: 'bot', password: 'wrong' },
        },
      ],
      send: () => undefined,
      requestTimeoutMs: 500,
    });
    try {
      expect(client.nodes[0]).toMatchObject({ status: 'disconnected' });
      expect(client.nodes[0]?.lastError).toContain('authentication_failed');
      expect(server.authorizationHeaders).toEqual([
        `Basic ${Buffer.from('bot:wrong').toString('base64')}`,
      ]);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('rejects an unsupported server protocol without retrying a known-incompatible node', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-v2', protocolVersion: 2 });
    const client = await RemediaClient.create({
      nodes: [
        {
          type: 'remote',
          id: 'remote-v2',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: () => undefined,
      requestTimeoutMs: 500,
    });
    try {
      expect(client.nodes[0]).toMatchObject({ status: 'disconnected' });
      expect(client.nodes[0]?.lastError).toContain('protocol 2 is unsupported');
      expect(server.authorizationHeaders).toHaveLength(1);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('rejects a configured Discord bot identity mismatch before sending OP 4', async () => {
    const server = await FakeServerNode.listen({
      id: 'remote-identity',
      botUserId: '100000000000000009',
    });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      nodes: [
        {
          type: 'remote',
          id: 'remote-a',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      await expect(voice.connect({ timeoutMs: 1_000 })).rejects.toMatchObject({
        name: 'DiscordBotMismatchError',
      });
      expect(payloads).toHaveLength(0);
    } finally {
      await client.close();
      await server.close();
    }
  });
});
