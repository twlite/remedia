import { describe, expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const GUILD_ID = '200000000000000001';
const CHANNEL_ID = '300000000000000001';
const BOT_ID = '100000000000000001';

describe('remote Discord playback', () => {
  it('requires a valid bot identity for remote nodes and identifies separate bots at handshake', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-bot-identities' });
    const makeClient = (botUserId: string) =>
      RemediaClient.create({
        botUserId,
        nodes: [
          {
            type: 'remote',
            id: 'shared-server',
            url: server.url,
            auth: { username: 'same-account', password: 'secret' },
          },
        ],
        send: () => undefined,
      });
    try {
      await expect(
        RemediaClient.create({
          nodes: [
            {
              type: 'remote',
              id: 'missing-id',
              url: server.url,
              auth: { username: 'same-account', password: 'secret' },
            },
          ],
          send: () => undefined,
        }),
      ).rejects.toThrow(/botUserId/);
      await expect(makeClient('0')).rejects.toThrow(/botUserId/);

      const first = await makeClient(BOT_ID);
      const secondId = '100000000000000009';
      const second = await makeClient(secondId);
      try {
        expect(first.nodes[0]?.status).toBe('healthy');
        expect(second.nodes[0]?.status).toBe('healthy');
        expect(server.botUserIdHeaders).toEqual([BOT_ID, secondId]);
      } finally {
        await Promise.all([first.close(), second.close()]);
      }
    } finally {
      await server.close();
    }
  });

  it('connects through host Gateway events and controls playback by generation', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-playback' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    let resolveJoin!: () => void;
    const joinSent = new Promise<void>((resolve) => {
      resolveJoin = resolve;
    });
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
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
      expect(server.botUserIdHeaders[0]).toBe(BOT_ID);
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
        self_deaf: true,
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
      await voice.filters.set('bass=gain=8');
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

  it('replaces a remote voice transport during region migration without stopping paused playback', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-migration' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-migration',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;

      let started = 0;
      voice.on('playbackStarted', () => (started += 1));
      await voice.play({ url: 'https://media.example.test/migrating.mp3' });
      await voice.pause();
      const sourceGeneration = voice.snapshot.generation;

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      const migrating = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 2);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-b',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await migrating;

      expect(voice.state).toBe('paused');
      expect(voice.snapshot.generation).toBe(sourceGeneration);
      expect(started).toBe(1);
      expect(server.voiceConnectRequests).toBe(2);
      expect(server.voiceConnectionCredentials.at(-1)?.sessionId).toBe('voice-session-b');
      expect(server.voiceDisconnectRequests).toBe(0);
      expect((await voice.refresh()).state).toBe('paused');

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 6,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 4,
        d: { guild_id: GUILD_ID, token: 'stale-token', endpoint: 'old.voice.test:443' },
      });
      await Promise.resolve();
      expect(payloads).toHaveLength(2);
      expect(server.voiceConnectRequests).toBe(2);
      expect(server.voiceConnectionCredentials.at(-1)?.sessionId).toBe('voice-session-b');
      await voice.disconnect();
      expect(server.voiceDisconnectRequests).toBe(1);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('uses a server update alone to rotate the token when the endpoint stays the same', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-server-only-update' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-server-only',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 1,
        d: { guild_id: GUILD_ID, token: 'initial-token', endpoint: 'same.voice.test:443' },
      });
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 2,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session',
        },
      });
      await connecting;
      await voice.play({ url: 'https://media.example.test/server-only-update.mp3' });
      await voice.pause();
      const generation = voice.snapshot.generation;

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'latest-token', endpoint: 'same.voice.test:443' },
      });
      const recovery = voice.connect({ timeoutMs: 2_000 });
      await recovery;

      expect(voice.connectionState).toBe('connected');
      expect(voice.state).toBe('paused');
      expect(voice.snapshot.generation).toBe(generation);
      expect(server.voiceConnectRequests).toBe(2);
      expect(server.voiceDisconnectRequests).toBe(0);
      expect(server.voiceConnectionCredentials.at(-1)).toEqual({
        sessionId: 'voice-session',
        token: 'latest-token',
        endpoint: 'same.voice.test:443',
      });
      await voice.disconnect();
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('supersedes an in-flight remote transport setup when newer Gateway credentials arrive', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-stale-connect' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    let releaseConnect!: () => void;
    const blockedConnect = new Promise<void>((resolve) => {
      releaseConnect = resolve;
    });
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-stale-connect',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;
      await voice.play({ url: 'https://media.example.test/stale-connect.mp3' });
      const generation = voice.snapshot.generation;

      server.voiceConnectGate = () => blockedConnect;
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await waitUntil(() => payloads.length === 2);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-b',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await waitUntil(() => server.voiceConnectRequests === 2);
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 6,
        d: { guild_id: GUILD_ID, token: 'voice-token-c', endpoint: 'three.voice.test:443' },
      });
      await waitUntil(() => server.voiceSuspendRequests >= 1);
      server.voiceConnectGate = undefined;
      releaseConnect();

      await waitUntil(() => server.voiceConnectRequests === 3);
      await waitUntil(() => voice.connectionState === 'connected');
      expect(server.voiceSuspendRequests).toBeGreaterThanOrEqual(1);
      expect(server.voiceDisconnectRequests).toBe(0);
      expect(voice.state).toBe('playing');
      expect(voice.snapshot.generation).toBe(generation);

      await voice.disconnect();
      expect(server.voiceDisconnectRequests).toBe(1);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('allows stop during voice migration and does not restart the stopped source', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-stop-during-migration' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-stop-migration',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;
      let started = 0;
      voice.on('playbackStarted', () => (started += 1));
      await voice.play({ url: 'https://media.example.test/stopped-during-migration.mp3' });

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      const migrating = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 2);
      await voice.stop();
      expect(voice.state).toBe('stopped');

      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-b',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await migrating;

      expect(voice.connectionState).toBe('connected');
      expect(voice.state).toBe('stopped');
      expect(started).toBe(1);
      // Configuration serialization may skip a superseded intermediate credential set.
      expect(server.voiceConnectRequests).toBeGreaterThanOrEqual(2);
      expect(server.voiceConnectRequests).toBeLessThanOrEqual(3);
      expect(server.voiceConnectionCredentials.at(-1)?.sessionId).toBe('voice-session-b');
      expect(server.voiceDisconnectRequests).toBe(0);
      await voice.disconnect();
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('allows stop during remote voice migration and leaves the replacement connection idle', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-stop-migration' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-stop-migration',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;
      let started = 0;
      voice.on('playbackStarted', () => (started += 1));
      await voice.play({ url: 'https://media.example.test/stop-during-migration.mp3' });

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      const migrating = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 2);
      await voice.stop();
      expect(voice.state).toBe('stopped');

      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-b',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await migrating;

      expect(voice.connectionState).toBe('connected');
      expect(voice.state).toBe('stopped');
      expect(started).toBe(1);
      // Configuration serialization may skip a superseded intermediate credential set.
      expect(server.voiceConnectRequests).toBeGreaterThanOrEqual(2);
      expect(server.voiceConnectRequests).toBeLessThanOrEqual(3);
      expect(server.voiceConnectionCredentials.at(-1)?.sessionId).toBe('voice-session-b');
      expect(server.voiceDisconnectRequests).toBe(0);
      await voice.disconnect();
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('fails playback only after bounded voice migration recovery fails', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-migration-failure' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-failure',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;
      let starts = 0;
      let failures = 0;
      voice.on('playbackStarted', () => {
        starts += 1;
      });
      voice.on('voiceFailed', () => {
        failures += 1;
      });
      await voice.play({ url: 'https://media.example.test/failing-migration.mp3' });
      const generation = voice.snapshot.generation;
      server.voiceConnectFailures = 3;

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await waitUntil(() => payloads.length === 2);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-b',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await waitUntil(() => payloads.length === 3);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 6,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-c',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 7,
        d: { guild_id: GUILD_ID, token: 'voice-token-c', endpoint: 'three.voice.test:443' },
      });
      await waitUntil(() => payloads.length === 4);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 8,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-d',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 9,
        d: { guild_id: GUILD_ID, token: 'voice-token-d', endpoint: 'four.voice.test:443' },
      });
      await waitUntil(() => voice.connectionState === 'failed');

      expect(server.voiceConnectRequests).toBe(4);
      expect(voice.snapshot.generation).toBe(generation);
      expect(starts).toBe(1);
      expect(failures).toBe(1);
      await voice.disconnect();
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('keeps an explicit leave authoritative during pending remote voice recovery', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-migration-cancel' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-cancel',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;

      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await waitUntil(() => payloads.length === 2);
      await voice.disconnect();
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'stale-session',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'stale-token', endpoint: 'old.voice.test:443' },
      });

      expect(payloads.at(-1)?.d.channel_id).toBeNull();
      expect(server.voiceConnectRequests).toBe(1);
      expect(server.voiceDisconnectRequests).toBe(1);
      expect(voice.connectionState).toBe('disconnected');
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('rejects a delayed remote replacement after an explicit disconnect', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-voice-generation-fence' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    let releaseConnect!: () => void;
    let connectStarted!: () => void;
    const blockedConnect = new Promise<void>((resolve) => {
      releaseConnect = resolve;
    });
    const started = new Promise<void>((resolve) => {
      connectStarted = resolve;
    });
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
      nodes: [
        {
          type: 'remote',
          id: 'remote-generation-fence',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send: (_guildId, payload) => {
        payloads.push(payload);
      },
      requestTimeoutMs: 2_000,
    });
    try {
      const voice = client.createDiscordSession({
        guildId: GUILD_ID,
        channelId: CHANNEL_ID,
        userId: BOT_ID,
      });
      const connecting = voice.connect({ timeoutMs: 2_000 });
      await waitUntil(() => payloads.length === 1);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 1,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-a',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 2,
        d: { guild_id: GUILD_ID, token: 'voice-token-a', endpoint: 'one.voice.test:443' },
      });
      await connecting;
      await voice.play({ url: 'https://media.example.test/generation-fence.mp3' });

      server.voiceConnectGate = async () => {
        connectStarted();
        await blockedConnect;
      };
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 3,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await waitUntil(() => payloads.length === 2);
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: 4,
        d: {
          guild_id: GUILD_ID,
          user_id: BOT_ID,
          channel_id: CHANNEL_ID,
          session_id: 'voice-session-b',
        },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: 5,
        d: { guild_id: GUILD_ID, token: 'voice-token-b', endpoint: 'two.voice.test:443' },
      });
      await started;

      await voice.disconnect();
      releaseConnect();
      await waitUntil(
        () =>
          server.sessions.values().next().value?.guilds.get(GUILD_ID)?.voiceState ===
          'disconnected',
      );

      expect(voice.connectionState).toBe('disconnected');
      expect(voice.state).toBe('stopped');
      expect(server.voiceConnectRequests).toBe(2);
      expect(server.voiceDisconnectRequests).toBe(1);
      expect(server.sessions.values().next().value?.guilds.get(GUILD_ID)?.voiceState).toBe(
        'disconnected',
      );
    } finally {
      releaseConnect();
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
      botUserId: BOT_ID,
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
      botUserId: BOT_ID,
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

  it('rejects a session bot identity mismatch before sending OP 4', async () => {
    const server = await FakeServerNode.listen({ id: 'remote-identity' });
    const payloads: DiscordGatewayVoicePayload[] = [];
    const client = await RemediaClient.create({
      botUserId: BOT_ID,
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
      expect(() =>
        client.createDiscordSession({
          guildId: GUILD_ID,
          channelId: CHANNEL_ID,
          userId: '100000000000000009',
        }),
      ).toThrow(/Discord bot user mismatch/);
      expect(payloads).toHaveLength(0);
    } finally {
      await client.close();
      await server.close();
    }
  });
});

async function waitUntil(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error('timed out waiting for Gateway voice request');
    await new Promise<void>((resolve) => setTimeout(resolve, 5));
  }
}
