import assert from 'node:assert/strict';
import { EventEmitter, once } from 'node:events';
import { afterAll, beforeAll, test } from 'vitest';

import { PlaybackSessionEvents, Remedia, VoiceSessionEvents } from '../lib/index.js';
type PlaybackSessionEventName = import('../lib/index.js').PlaybackSessionEvents;
const typedPlaybackEvent: PlaybackSessionEventName = PlaybackSessionEvents.playbackCompleted;
import { createHeldAudioServer, createWaveFixture, makeWave, waitFor } from './helpers.js';

let fixture: Awaited<ReturnType<typeof createWaveFixture>>;

beforeAll(async () => {
  fixture = await createWaveFixture(0.04);
});

afterAll(async () => {
  await fixture?.cleanup();
});

test('emits generation-correlated lifecycle events and retains arbitrary metadata by identity', async () => {
  assert.equal(typedPlaybackEvent, PlaybackSessionEvents.playbackCompleted);
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession<{ track: number; marker(): string }>();
  const metadata = { track: 1, marker: () => 'local-only' };
  const events: string[] = [];
  let started: unknown;
  let completed: unknown;
  const completion = once(playback, PlaybackSessionEvents.playbackCompleted);
  const aggregate = once(remedia, PlaybackSessionEvents.playbackStarted);

  assert.ok(playback instanceof EventEmitter);
  playback.on(PlaybackSessionEvents.playbackStarted, (event) => {
    events.push(PlaybackSessionEvents.playbackStarted);
    started = event;
  });
  playback.on(PlaybackSessionEvents.playbackCompleted, (event) => {
    events.push(PlaybackSessionEvents.playbackCompleted);
    completed = event;
  });
  playback.hooks.beforePlay(({ customMetadata }) => {
    assert.equal(customMetadata, metadata);
  });

  try {
    await playback.play({ buffer: fixture.wave }, { customMetadata: metadata });
    await completion;

    assert.deepEqual(events, [
      PlaybackSessionEvents.playbackStarted,
      PlaybackSessionEvents.playbackCompleted,
    ]);
    assert.equal((started as { customMetadata: unknown }).customMetadata, metadata);
    assert.equal((completed as { customMetadata: unknown }).customMetadata, metadata);
    assert.equal((completed as { generation: number }).generation, playback.currentGeneration);
    assert.equal((await aggregate)[0].customMetadata, metadata);

    const disposed = once(remedia, PlaybackSessionEvents.sessionDisposed);
    await playback.dispose();
    assert.equal((await disposed)[0].sessionId, playback.sessionId);
    assert.equal(playback.listenerCount(PlaybackSessionEvents.playbackStarted), 1);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('emits pause/resume and configuration transitions only after successful changes', async () => {
  const held = await createHeldAudioServer(makeWave(5));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  const events: string[] = [];
  let completed = 0;
  playback.on(PlaybackSessionEvents.playbackPaused, () =>
    events.push(PlaybackSessionEvents.playbackPaused),
  );
  playback.on(PlaybackSessionEvents.playbackResumed, () =>
    events.push(PlaybackSessionEvents.playbackResumed),
  );
  playback.on(PlaybackSessionEvents.filterChanged, () =>
    events.push(PlaybackSessionEvents.filterChanged),
  );
  playback.on(PlaybackSessionEvents.volumeChanged, () =>
    events.push(PlaybackSessionEvents.volumeChanged),
  );
  playback.on(PlaybackSessionEvents.playbackCompleted, () => completed++);

  try {
    const starting = playback.play({ url: held.url });
    await held.ready;
    await starting;
    await playback.pause();
    await playback.pause();
    await playback.resume();
    await playback.filters.set('anull');
    await playback.filters.set('anull');
    await playback.setVolume(0.75);
    await playback.setVolume(0.75);
    assert.deepEqual(events, [
      PlaybackSessionEvents.playbackPaused,
      PlaybackSessionEvents.playbackResumed,
      PlaybackSessionEvents.filterChanged,
      PlaybackSessionEvents.volumeChanged,
    ]);

    const stopped = once(playback, PlaybackSessionEvents.playbackStopped);
    await playback.stop();
    await stopped;
    assert.equal(completed, 0);
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});

test('pre-play hooks run in registration order and cancellation starts no media', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession<{ allowed: boolean }>();
  const order: number[] = [];
  let started = 0;
  playback.on(PlaybackSessionEvents.playbackStarted, () => started++);
  playback.hooks.beforePlay(async ({ customMetadata, source, signal }) => {
    order.push(1);
    assert.equal(customMetadata?.allowed, false);
    assert.ok('buffer' in source);
    assert.equal(signal.aborted, false);
  });
  playback.hooks.beforePlay(() => {
    order.push(2);
    return { cancel: true, reason: 'blocked by application policy' };
  });
  playback.hooks.beforePlay(() => {
    order.push(3);
  });

  try {
    await assert.rejects(
      playback.play({ buffer: fixture.wave }, { customMetadata: { allowed: false } }),
      { name: 'AbortError', message: 'blocked by application policy' },
    );
    assert.deepEqual(order, [1, 2]);
    assert.equal(started, 0);
    assert.equal(playback.state, 'idle');
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('disposing a session aborts a pending asynchronous pre-play hook', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  playback.hooks.beforePlay(() => new Promise<void>(() => undefined));
  const pending = playback.play({ buffer: fixture.wave }).then(
    () => ({ started: true }),
    (error: unknown) => ({ error }),
  );

  try {
    await playback.dispose();
    const result = await Promise.race([
      pending,
      new Promise<{ timeout: true }>((resolve) =>
        setTimeout(() => resolve({ timeout: true }), 500),
      ),
    ]);
    assert.ok('error' in result);
    assert.equal((result.error as Error).name, 'AbortError');
  } finally {
    await remedia.close();
  }
});

test('voice session state transitions are typed and disposal is emitted once', async () => {
  const sent: unknown[] = [];
  const remedia = await Remedia.create({
    send: (_guild, payload) => {
      sent.push(payload);
    },
  });
  const voice = remedia.createDiscordSession({
    guildId: '200000000000000001',
    channelId: '300000000000000001',
    userId: '100000000000000001',
  });
  const states: string[] = [];
  voice.on(VoiceSessionEvents.voiceStateChanged, ({ state }) => states.push(state));
  const controller = new AbortController();
  const connecting = voice.connect({ signal: controller.signal });
  await waitFor(() => (sent.length ? true : undefined));
  controller.abort();
  await assert.rejects(connecting, { name: 'AbortError' });
  assert.deepEqual(states, ['connecting', 'disconnected']);

  const disposed = once(voice, PlaybackSessionEvents.sessionDisposed);
  await voice.dispose();
  await disposed;
  await remedia.close();
});
