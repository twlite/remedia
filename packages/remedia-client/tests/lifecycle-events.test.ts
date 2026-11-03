import { describe, expect, it } from 'vitest';
import {
  RemediaClient,
  PlaybackSessionEvents,
  RemediaClientEventNames,
  VoiceSessionEvents,
} from '../lib/index.js';
type PlaybackSessionEventName = import('../lib/index.js').PlaybackSessionEvents;
type ClientEventName = import('../lib/index.js').RemediaClientEventNames;
const typedPlaybackEvent: PlaybackSessionEventName = PlaybackSessionEvents.playbackCompleted;
const typedClientEvent: ClientEventName = RemediaClientEventNames.sessionLost;
import type { DiscordGatewayVoicePayload } from '../lib/types.js';
import { FakeServerNode } from './helpers.js';

const GUILD_ID = '200000000000000101';
const CHANNEL_ID = '300000000000000101';
const BOT_ID = '100000000000000001';

interface EventSample {
  generation: number;
  customMetadata?: unknown;
  nodeId?: string | null;
  reconstructed?: boolean;
}

describe('hybrid client lifecycle events and pre-play hooks', () => {
  it('correlates early terminal events with metadata and ignores stale generations', async () => {
    expect(typedPlaybackEvent).toBe(PlaybackSessionEvents.playbackCompleted);
    expect(typedClientEvent).toBe(RemediaClientEventNames.sessionLost);
    const { client, server, voice } = await createConnectedRemoteSession('event-correlation');
    const order: string[] = [];
    const firstMetadata = { track: 1, marker: () => 'kept in this process' };
    const secondMetadata = { track: 2 };
    let firstStart: EventSample | undefined;
    let firstCompletion: EventSample | undefined;
    let secondCompletion: EventSample | undefined;
    let aggregateStart: EventSample | undefined;

    voice.on(PlaybackSessionEvents.playbackStarted, (event) => {
      order.push(`${event.generation}:started`);
      if (event.generation === 1) firstStart = event;
    });
    voice.on(PlaybackSessionEvents.playbackCompleted, (event) => {
      order.push(`${event.generation}:completed`);
      if (event.generation === 1) firstCompletion = event;
      if (event.generation === 2) secondCompletion = event;
    });
    client.on(PlaybackSessionEvents.playbackStarted, (event) => {
      if (event.generation === 1) aggregateStart = event;
    });

    try {
      server.finishBeforePlayResponse = true;
      await voice.play(
        { url: 'https://media.example.test/one.mp3' },
        { customMetadata: firstMetadata },
      );
      expect(firstStart?.customMetadata).toBe(firstMetadata);
      expect(firstCompletion?.customMetadata).toBe(firstMetadata);
      expect(firstStart?.generation).toBe(1);
      expect(firstCompletion?.generation).toBe(1);
      expect(aggregateStart?.customMetadata).toBe(firstMetadata);
      expect(order).toEqual(['1:started', '1:completed']);

      server.finishBeforePlayResponse = false;
      await voice.play(
        { url: 'https://media.example.test/two.mp3' },
        { customMetadata: secondMetadata },
      );
      server.sendEvent(GUILD_ID, 'player.completed', { generation: 1 });
      expect(order).toEqual(['1:started', '1:completed', '2:started']);

      await voice.pause();
      await voice.resume();
      await voice.seek(900);
      await voice.filters.set('bass=g=8');
      await voice.setVolume(0.75);
      server.setPlaybackState(GUILD_ID, 'completed');
      server.sendEvent(GUILD_ID, 'player.completed', { generation: 2 });
      await waitUntil(() => secondCompletion !== undefined);
      expect(secondCompletion?.customMetadata).toBe(secondMetadata);
      expect(secondCompletion?.nodeId).toBe('event-correlation');
      expect(order).toEqual(['1:started', '1:completed', '2:started', '2:completed']);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('runs local hooks in order and does not call remote playback after cancellation or failure', async () => {
    const { client, server, voice } = await createConnectedRemoteSession('event-hooks');
    const order: number[] = [];
    voice.hooks.beforePlay(async ({ source, customMetadata, signal }) => {
      order.push(1);
      expect('url' in source).toBe(true);
      expect(customMetadata).toEqual({ allowed: false });
      expect(signal.aborted).toBe(false);
    });
    voice.hooks.beforePlay(() => {
      order.push(2);
      return { cancel: true, reason: 'application policy' };
    });
    voice.hooks.beforePlay(() => {
      order.push(3);
    });

    try {
      const playsBefore = server.playerPlayRequests;
      await expect(
        voice.play(
          { url: 'https://media.example.test/blocked.mp3' },
          { customMetadata: { allowed: false } },
        ),
      ).rejects.toMatchObject({ name: 'AbortError', message: 'application policy' });
      expect(order).toEqual([1, 2]);
      expect(server.playerPlayRequests).toBe(playsBefore);

      voice.hooks.clear();
      voice.hooks.beforePlay(() => {
        throw new Error('hook failed');
      });
      await expect(voice.play({ url: 'https://media.example.test/error.mp3' })).rejects.toThrow(
        'hook failed',
      );
      expect(server.playerPlayRequests).toBe(playsBefore);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('aborts a hanging hook when the session is disposed', async () => {
    const { client, server, voice } = await createConnectedRemoteSession('event-hook-dispose');
    voice.hooks.beforePlay(() => new Promise<void>(() => undefined));
    const pending = voice.play({ url: 'https://media.example.test/hang.mp3' }).then(
      () => ({ started: true }),
      (error: unknown) => ({ error }),
    );

    try {
      const disposed = new Promise<void>((resolve) =>
        voice.once(PlaybackSessionEvents.sessionDisposed, () => resolve()),
      );
      await voice.dispose();
      await disposed;
      expect(voice.listenerCount(PlaybackSessionEvents.playbackStarted)).toBe(0);
      const result = await Promise.race([
        pending,
        new Promise<{ timeout: true }>((resolve) =>
          setTimeout(() => resolve({ timeout: true }), 500),
        ),
      ]);
      expect(result).toHaveProperty('error');
      expect((result as { error: Error }).error.name).toBe('AbortError');
      expect(server.playerPlayRequests).toBe(0);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('keeps the play AbortSignal bound until remote playback reaches a terminal state', async () => {
    const { client, server, voice } = await createConnectedRemoteSession('event-play-abort');
    const controller = new AbortController();

    try {
      await voice.play(
        { url: 'https://media.example.test/abort-after-start.mp3' },
        { signal: controller.signal },
      );
      expect(voice.state).toBe('playing');

      const completion = voice.wait();
      controller.abort(new Error('caller cancelled playback'));

      await expect(completion).resolves.toBe('stopped');
      expect(voice.state).toBe('stopped');
      expect(server.playerPlayRequests).toBe(1);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('reconstructs one terminal event after reconnect when it was missed while detached', async () => {
    const { client, server, voice } = await createConnectedRemoteSession('event-reconcile');
    let reconstructed: EventSample | undefined;
    voice.on(PlaybackSessionEvents.playbackCompleted, (event) => {
      if (event.generation === 1) reconstructed = event;
    });

    try {
      await voice.play(
        { url: 'https://media.example.test/reconnect.mp3' },
        {
          customMetadata: { source: 'local-js' },
        },
      );
      server.setPlaybackState(GUILD_ID, 'completed');
      server.closeSockets();
      await waitUntil(() => reconstructed?.reconstructed === true);
      expect(reconstructed?.customMetadata).toEqual({ source: 'local-js' });
      expect(reconstructed?.generation).toBe(1);
      await waitUntil(() => client.nodes[0]?.status === 'healthy');
      expect(reconstructed?.reconstructed).toBe(true);
      await voice.refresh();
      expect(reconstructed?.reconstructed).toBe(true);
    } finally {
      await client.close();
      await server.close();
    }
  });

  it('emits a voice failure and session loss without fabricating playback completion', async () => {
    const server = await FakeServerNode.listen({
      id: 'event-loss',
      dropSessionsOnDisconnect: true,
    });
    const voiceStates: string[] = [];
    const { client, voice } = await createConnectedRemoteSessionWithServer(server);
    let voiceFailed = 0;
    voice.on(VoiceSessionEvents.voiceStateChanged, ({ state }) => voiceStates.push(state));
    voice.on(VoiceSessionEvents.voiceFailed, () => voiceFailed++);
    let completed = 0;
    voice.on(PlaybackSessionEvents.playbackCompleted, () => completed++);

    try {
      await voice.play({ url: 'https://media.example.test/lost.mp3' });
      server.closeSockets();
      await waitUntil(() => voice.connectionState === 'failed');
      expect(voiceFailed).toBe(1);
      expect(completed).toBe(0);
      expect(voiceStates).toContain('failed');
    } finally {
      await client.close();
      await server.close();
    }
  });
});

async function createConnectedRemoteSession(id: string) {
  const server = await FakeServerNode.listen({ id });
  const connected = await createConnectedRemoteSessionWithServer(server);
  return { ...connected, server };
}

async function createConnectedRemoteSessionWithServer(server: FakeServerNode) {
  const payloads: DiscordGatewayVoicePayload[] = [];
  let join!: () => void;
  const joinSent = new Promise<void>((resolve) => {
    join = resolve;
  });
  const client = await RemediaClient.create({
    nodes: [
      {
        type: 'remote',
        id: server.id,
        url: server.url,
        auth: { username: 'bot', password: 'secret' },
      },
    ],
    send: (_guildId, payload) => {
      payloads.push(payload);
      join();
    },
    requestTimeoutMs: 2_000,
  });
  const voice = client.createDiscordSession({
    guildId: GUILD_ID,
    channelId: CHANNEL_ID,
    userId: BOT_ID,
  });
  const connecting = voice.connect({ timeoutMs: 2_000 });
  await joinSent;
  client.receive({
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
    t: 'VOICE_SERVER_UPDATE',
    s: 2,
    d: {
      guild_id: GUILD_ID,
      token: 'voice-token',
      endpoint: 'voice.example.discord.media:443',
    },
  });
  await connecting;
  return { client, voice, payloads };
}

async function waitUntil(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error('timed out waiting for a lifecycle event');
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}
