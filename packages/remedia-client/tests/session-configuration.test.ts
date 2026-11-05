import { expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const guildId = '200000000000000001';
const channelId = '300000000000000001';
const userId = '100000000000000001';

async function harness(server: FakeServerNode, bot = userId) {
  const sent: DiscordGatewayVoicePayload[] = [];
  const client = await RemediaClient.create({
    botUserId: bot,
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
  let sequence = 0;
  return {
    client,
    sent,
    credentials(destination: string) {
      client.receive({
        t: 'VOICE_STATE_UPDATE',
        s: ++sequence,
        d: { guild_id: guildId, channel_id: destination, user_id: bot, session_id: 'same-session' },
      });
      client.receive({
        t: 'VOICE_SERVER_UPDATE',
        s: ++sequence,
        d: { guild_id: guildId, token: `token-${sequence}`, endpoint: 'voice.example.test:443' },
      });
    },
    async requested(destination: string) {
      await expect.poll(() => sent.at(-1)?.d.channel_id).toBe(destination);
      this.credentials(destination);
    },
  };
}

it('isolates two bots in the same guild/channel, applies initial settings, and cleans assignments', async () => {
  const server = await FakeServerNode.listen({ id: 'configuration' });
  const a = await harness(server);
  const b = await harness(server, '100000000000000002');
  try {
    const first = a.client.createDiscordSession({
      guildId,
      channelId,
      userId,
      volume: 0.4,
      filterGraph: 'anull',
      selfDeaf: false,
      selfMute: true,
    });
    const second = b.client.createDiscordSession({
      guildId,
      channelId,
      userId: '100000000000000002',
    });
    expect(() => a.client.createDiscordSession({ guildId, channelId, userId })).toThrow(
      /already exists/,
    );
    expect(first.volume).toBe(0.4);
    expect(first.filters.graph).toBe('anull');
    const joins = Promise.all([first.connect(), second.connect()]);
    await Promise.all([a.requested(channelId), b.requested(channelId)]);
    await joins;
    const guilds = [...server.sessions.values()].map((s) => s.guilds.get(guildId)!);
    expect(guilds[0]!.playback).toMatchObject({ volume: 0.4, filterGraph: 'anull', generation: 0 });
    expect(guilds[1]!.playback).toMatchObject({ volume: 1, filterGraph: null, generation: 0 });
    await first.play({ url: 'https://media.example.test/song.mp3' });
    await first.pause();
    const generation = first.snapshot.generation;
    const terminals: unknown[] = [];
    first.on('playbackStopped', (e) => terminals.push(e));
    first.on('playbackCompleted', (e) => terminals.push(e));
    first.on('playbackFailed', (e) => terminals.push(e));
    const move = first.move('300000000000000002');
    await a.requested('300000000000000002');
    await move;
    expect(first.state).toBe('paused');
    expect(first.snapshot.generation).toBe(generation);
    expect(first.volume).toBe(0.4);
    expect(first.filters.graph).toBe('anull');
    expect(second.state).toBe('idle');
    expect(terminals).toEqual([]);
    const count = a.sent.length;
    await first.move(first.channelId);
    await first.setSelfDeaf(false);
    expect(a.sent).toHaveLength(count);
    await first.setSelfMute(false);
    expect(a.sent.at(-1)?.d).toMatchObject({ self_deaf: false, self_mute: false });
    await first.dispose();
    expect(a.client.assignments).toEqual([]);
    expect(server.allocatedGuildCount).toBe(1);
    expect(second.connectionState).toBe('connected');
    await second.dispose();
    expect(server.allocatedGuildCount).toBe(0);
  } finally {
    await Promise.all([a.client.close(), b.client.close()]);
    await server.close();
  }
});

it('supersedes moves, ignores old-channel state, and disconnects pending movement', async () => {
  const server = await FakeServerNode.listen({ id: 'moves' });
  const h = await harness(server);
  try {
    const session = h.client.createDiscordSession({ guildId, channelId, userId });
    const join = session.connect();
    await h.requested(channelId);
    await join;
    const old = session.move('300000000000000002');
    const rejected = expect(old).rejects.toThrow(/superseded/);
    const latest = session.move('300000000000000003');
    h.credentials(channelId);
    await h.requested('300000000000000003');
    await latest;
    await rejected;
    expect(session.channelId).toBe('300000000000000003');
    const pending = session.move(channelId);
    const cancelled = expect(pending).rejects.toThrow(/disconnect/);
    await session.dispose();
    await cancelled;
    expect(h.client.assignments).toEqual([]);
    expect(server.allocatedGuildCount).toBe(0);
  } finally {
    await h.client.close();
    await server.close();
  }
});

it('validates declarative configuration before registering a guild', async () => {
  const server = await FakeServerNode.listen({ id: 'validation' });
  const h = await harness(server);
  try {
    for (const volume of [-1, 3, NaN, Infinity])
      expect(() => h.client.createDiscordSession({ guildId, channelId, userId, volume })).toThrow(
        /volume/,
      );
    expect(() =>
      h.client.createDiscordSession({ guildId, channelId, userId, filterGraph: ' ' }),
    ).toThrow(/filterGraph/);
    const session = h.client.createDiscordSession({ guildId, channelId, userId });
    await session.dispose();
  } finally {
    await h.client.close();
    await server.close();
  }
});

it('serializes initialization and live configuration without emitting unconfigured playback', async () => {
  const server = await FakeServerNode.listen({ id: 'initialization-race' });
  const h = await harness(server);
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.voiceConnectGate = () => gate;
  try {
    const session = h.client.createDiscordSession({
      guildId,
      channelId,
      userId,
      volume: 0.3,
      filterGraph: 'anull',
    });
    const joined = session.connect();
    await h.requested(channelId);
    await expect.poll(() => server.voiceConnectRequests).toBe(1);
    const configured = session.setVolume(0.8);
    release();
    await Promise.all([joined, configured]);
    await session.play({ url: 'https://media.example.test/track.mp3' });
    expect(session.volume).toBe(0.8);
    expect(session.filters.graph).toBe('anull');
    await session.dispose();
  } finally {
    release();
    await h.client.close();
    await server.close();
  }
});

it('a failed move parks playback and a fresh connect can retry the destination', async () => {
  const server = await FakeServerNode.listen({ id: 'failed-move' });
  const h = await harness(server);
  try {
    const session = h.client.createDiscordSession({ guildId, channelId, userId });
    const joined = session.connect();
    await h.requested(channelId);
    await joined;
    await session.play({ url: 'https://media.example.test/track.mp3' });
    const generation = session.snapshot.generation;
    server.voiceConnectFailures = 1;
    const move = session.move('300000000000000002');
    const failed = expect(move).rejects.toThrow();
    await h.requested('300000000000000002');
    await failed;
    expect(session.connectionState).toBe('failed');
    expect(session.state).toBe('playing');
    const retry = session.connect();
    await h.requested(session.channelId);
    await retry;
    expect(session.snapshot.generation).toBe(generation);
    expect(server.playerPlayRequests).toBe(1);
    await session.dispose();
  } finally {
    await h.client.close();
    await server.close();
  }
});

it('disposal wins backend allocation and repeated disposal releases registration once', async () => {
  const server = await FakeServerNode.listen({ id: 'allocation-cleanup' });
  const h = await harness(server);
  try {
    const session = h.client.createDiscordSession({ guildId, channelId, userId });
    let disposed = 0;
    session.on('sessionDisposed', () => {
      disposed++;
    });
    const connecting = session.connect();
    const cancelled = expect(connecting).rejects.toThrow(/disconnect/);
    const disposing = session.dispose();
    expect(session.dispose()).toBe(disposing);
    await disposing;
    await cancelled;
    expect(disposed).toBe(1);
    expect(h.client.assignments).toEqual([]);
    expect(server.allocatedGuildCount).toBe(0);
    const replacement = h.client.createDiscordSession({ guildId, channelId, userId });
    await replacement.dispose();
  } finally {
    await h.client.close();
    await server.close();
  }
});
