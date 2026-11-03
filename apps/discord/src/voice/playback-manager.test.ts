import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { test } from 'node:test';
import {
  PlaybackSessionEvents,
  VoiceSessionEvents,
  type DiscordSession,
  type PlaybackState,
  type RemediaClient,
} from '@remedia/client';
import type { EmbedBuilder, SendableChannels } from 'discord.js';
import { displayTrack } from './command-helpers.js';
import { FILTER_PRESETS, FILTER_PRESET_NAMES } from './filter-presets.js';
import { DiscordPlaybackManager } from './playback-manager.js';

test('uses the Remedia title when available and a sanitized URL otherwise', () => {
  assert.equal(
    displayTrack({
      url: 'https://media.example/one.mp3?token=private',
      info: { metadata: { Title: '  A track title  ' } },
    }),
    'A track title',
  );
  assert.equal(
    displayTrack({ url: 'https://user:password@media.example/one.mp3?token=private' }),
    'https://media.example/one.mp3',
  );
});

test('keeps the six named presets in the Discord harness only', () => {
  assert.deepEqual(FILTER_PRESET_NAMES, [
    '8d',
    'bassboost',
    'nightcore',
    'vaporwave',
    'tremolo',
    'vibrato',
  ]);
  assert.match(FILTER_PRESETS['8d'], /apulsator/);
  assert.match(FILTER_PRESETS.nightcore, /asetrate=60000/);
  assert.match(FILTER_PRESETS.vaporwave, /asetrate=38400/);
});

test('command actions suppress duplicate lifecycle posts while automatic starts remain attributed', async () => {
  const { manager, session } = createManager();
  const origin = createTextChannel();
  const other = createTextChannel();
  await manager.enqueue(
    'guild',
    'channel',
    'bot',
    { url: 'https://media.example/one.mp3', requestedBy: 'alice' },
    origin.channel,
  );
  assert.equal(session.playMetadata[0]?.textChannel, origin.channel);

  await manager.enqueue(
    'guild',
    'channel',
    'bot',
    { url: 'https://media.example/two.mp3', requestedBy: 'bob' },
    other.channel,
  );
  await manager.pause('guild', 'channel');
  await manager.resume('guild', 'channel');
  await manager.seek('guild', 'channel', 12_000);
  await manager.setFilters('guild', 'channel', ['tremolo', 'vibrato']);
  await manager.setVolume('guild', 'channel', 80);
  session.finish('completed');
  await new Promise<void>((resolve) => setImmediate(resolve));

  const titles = origin.messages.map(embedTitle);
  assert.deepEqual(titles, ['Now playing']);
  const notification = origin.messages[0]?.embeds?.[0] as EmbedBuilder | undefined;
  assert.ok(notification?.data.description?.includes('Example title'));
  assert.ok(
    notification?.data.fields?.some(
      (field) => field.name === 'Requested by' && field.value === '<@bob>',
    ),
  );
  assert.equal(notification?.data.footer?.text, 'Automatic playback notification');
  assert.equal(other.messages.length, 0);

  await manager.disconnect('guild', 'channel');
  const sentBeforeLateEvent = origin.messages.length;
  session.emitVoiceFailure(new Error('late failure'));
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(origin.messages.length, sentBeforeLateEvent);
  await manager.close();
});

test('announces a source failure when playback rejects before a start event', async () => {
  const { manager, session } = createManager();
  const channel = createTextChannel();
  const url = 'https://media.example/unreachable.mp3?token=secret';
  session.failingUrls.add(url);

  await assert.rejects(
    manager.enqueue('guild', 'channel', 'bot', { url, requestedBy: 'alice' }, channel.channel),
  );
  assert.equal(
    channel.messages.filter((message) => embedTitle(message) === 'Playback failed').length,
    1,
  );
  assert.ok(channel.messages.every((message) => !embedDescription(message).includes('secret')));
  await manager.close();
});

test('queues tracks, advances on completion, and leaves voice connected when stopping', async () => {
  const { manager, session } = createManager();

  const first = await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/one.mp3',
    requestedBy: 'alice',
  });
  assert.equal(first.started, true);
  assert.deepEqual(session.playedUrls, ['https://media.example/one.mp3']);
  await manager.setFilters('guild', 'channel', ['bassboost', 'tremolo']);
  await manager.setVolume('guild', 'channel', 75);

  const second = await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/two.mp3',
    requestedBy: 'bob',
  });
  assert.equal(second.started, false);
  assert.equal(second.queuePosition, 1);

  const pause = await manager.pauseWithContext('guild', 'channel');
  assert.equal(pause.previousState, 'playing');
  assert.equal(pause.changed, true);
  assert.equal(pause.current?.url, 'https://media.example/one.mp3');
  assert.equal(session.state, 'paused');
  const resume = await manager.resumeWithContext('guild', 'channel');
  assert.equal(resume.previousState, 'paused');
  assert.equal(resume.current?.url, 'https://media.example/one.mp3');
  session.position = {
    mediaMs: 8_400,
    mediaPositionAccuracy: 'estimated',
    submittedMs: 6_720,
    playedMs: null,
  };
  const seek = await manager.seekWithContext('guild', 'channel', 12_500);
  assert.equal(seek.current.url, 'https://media.example/one.mp3');
  assert.equal(seek.previousMediaPositionMs, 8_400);
  assert.equal(seek.previousPositionEstimated, true);
  assert.equal(session.seekPositionMs, 12_500);

  session.finish('completed');
  await Promise.resolve();
  const snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.current?.url, 'https://media.example/two.mp3');
  assert.deepEqual(session.playedUrls, [
    'https://media.example/one.mp3',
    'https://media.example/two.mp3',
  ]);
  assert.equal(session.filters.graph, `${FILTER_PRESETS.bassboost},${FILTER_PRESETS.tremolo}`);
  assert.deepEqual((await manager.snapshot('guild', 'channel')).filterPresets, [
    'bassboost',
    'tremolo',
  ]);
  assert.equal(session.volume, 0.75);

  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/three.mp3',
    requestedBy: 'alice',
  });
  const skipped = await manager.skip('guild', 'channel');
  assert.equal(skipped?.url, 'https://media.example/three.mp3');

  await manager.stop('guild', 'channel');
  const stopped = await manager.snapshot('guild', 'channel');
  assert.equal(stopped.current, null);
  assert.equal(session.state, 'stopped');
  assert.equal(session.connectionState, 'connected');

  await manager.disconnect('guild', 'channel');
  assert.equal(session.disposed, true);
  await manager.close();
});

test('consecutive skips start tracks once and preserve combined per-guild effects', async () => {
  const { manager, session } = createManager();
  for (const name of ['a', 'b', 'c']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  await manager.setFilters('guild', 'channel', ['nightcore', 'tremolo']);
  await manager.setVolume('guild', 'channel', 150);

  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/b.wav');
  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/c.wav');
  assert.equal(session.playedUrls.filter((url) => url.endsWith('/b.wav')).length, 1);
  assert.equal(session.playedUrls.filter((url) => url.endsWith('/c.wav')).length, 1);
  assert.equal(session.filters.graph, 'asetrate=60000,aresample=48000,tremolo=f=5:d=0.6');
  assert.equal(session.volume, 1.5);
  session.finish('completed');
  await Promise.resolve();
  const snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.current, null);
  assert.equal(snapshot.queue.length, 0);
  assert.equal(snapshot.connectionState, 'connected');
  await manager.close();
});

test('track loop repeats only the current track and off resumes the queue', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/a.wav',
    requestedBy: 'alice',
  });
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/b.wav',
    requestedBy: 'alice',
  });
  assert.equal((await manager.snapshot('guild', 'channel')).loopMode, 'off');

  await manager.setLoopMode('guild', 'channel', 'track');
  let snapshot = await completeAndSnapshot(manager, session);
  assert.equal(snapshot.loopMode, 'track');
  assert.equal(snapshot.current?.url, 'https://media.example/a.wav');
  assert.deepEqual(
    snapshot.queue.map((track) => track.url),
    ['https://media.example/b.wav'],
  );
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/a.wav',
  ]);

  await manager.setLoopMode('guild', 'channel', 'off');
  snapshot = await completeAndSnapshot(manager, session);
  assert.equal(snapshot.current?.url, 'https://media.example/b.wav');
  assert.deepEqual(snapshot.queue, []);
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/a.wav',
    'https://media.example/b.wav',
  ]);
  await manager.close();
});

test('queue loop rotates completed and skipped tracks and remains guild-scoped', async () => {
  const { manager, session } = createManager();
  for (const name of ['a', 'b', 'c']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  await manager.setLoopMode('guild', 'channel', 'queue');

  let snapshot = await completeAndSnapshot(manager, session);
  assert.equal(snapshot.current?.url, 'https://media.example/b.wav');
  assert.deepEqual(
    snapshot.queue.map((track) => track.url),
    ['https://media.example/c.wav', 'https://media.example/a.wav'],
  );

  snapshot = await completeAndSnapshot(manager, session);
  assert.equal(snapshot.current?.url, 'https://media.example/c.wav');
  assert.deepEqual(
    snapshot.queue.map((track) => track.url),
    ['https://media.example/a.wav', 'https://media.example/b.wav'],
  );

  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/a.wav');
  snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.loopMode, 'queue');
  assert.deepEqual(
    snapshot.queue.map((track) => track.url),
    ['https://media.example/b.wav', 'https://media.example/c.wav'],
  );
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/b.wav',
    'https://media.example/c.wav',
    'https://media.example/a.wav',
  ]);
  await manager.close();
});

test('skipping in track loop advances instead of replaying the current track', async () => {
  const { manager, session } = createManager();
  for (const name of ['a', 'b']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  await manager.setLoopMode('guild', 'channel', 'track');

  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/b.wav');
  assert.deepEqual((await manager.snapshot('guild', 'channel')).queue, []);
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/b.wav',
  ]);
  await manager.close();
});

test('loop mode does not requeue failed tracks and stop does not restart playback', async () => {
  const { manager, session } = createManager();
  session.failingUrls.add('https://media.example/failing.wav');
  await assert.rejects(
    manager.enqueue('guild', 'channel', 'bot', {
      url: 'https://media.example/failing.wav',
      requestedBy: 'alice',
    }),
    /media source was not found/i,
  );
  await manager.setLoopMode('guild', 'channel', 'track');
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/next.wav',
    requestedBy: 'alice',
  });
  session.finish('failed');
  await new Promise<void>((resolve) => setImmediate(resolve));

  const afterFailure = await manager.snapshot('guild', 'channel');
  assert.equal(afterFailure.current, null);
  assert.equal(afterFailure.state, 'failed');
  assert.equal(afterFailure.loopMode, 'track');

  await manager.stop('guild', 'channel');
  const stopped = await manager.snapshot('guild', 'channel');
  assert.equal(stopped.current, null);
  assert.equal(stopped.loopMode, 'track');
  assert.deepEqual(session.playedUrls, [
    'https://media.example/failing.wav',
    'https://media.example/next.wav',
  ]);
  await manager.close();
});

test('now-playing snapshots prefer original media position over filtered output time', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/nightcore.wav',
    requestedBy: 'alice',
  });
  session.position = {
    mediaMs: 48_000,
    mediaPositionAccuracy: 'estimated',
    submittedMs: 38_400,
    playedMs: null,
  };

  const snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.positionMs, 48_000);
  assert.equal(snapshot.positionEstimated, true);
  await manager.close();
});

test('skip while paused and rapid skips advance serially', async () => {
  const { manager, session } = createManager();
  for (const name of ['a', 'b', 'c']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  await manager.pause('guild', 'channel');
  const results = await Promise.all([
    manager.skip('guild', 'channel'),
    manager.skip('guild', 'channel'),
  ]);
  assert.deepEqual(
    results.map((track) => track?.url),
    ['https://media.example/b.wav', 'https://media.example/c.wav'],
  );
  assert.equal(session.state, 'playing');
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/b.wav',
    'https://media.example/c.wav',
  ]);
  await manager.close();
});

test('skip during voice recovery preserves the next item and starts it after reconnection', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/a.wav',
    requestedBy: 'alice',
  });
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/b.wav',
    requestedBy: 'bob',
  });
  session.connectionState = 'reconnecting';
  const result = await manager.skipWithContext('guild', 'channel');
  assert.equal(result.skipped.url, 'https://media.example/a.wav');
  assert.equal(result.next, null);
  assert.equal(result.queuedNext?.url, 'https://media.example/b.wav');
  assert.deepEqual(session.playedUrls, ['https://media.example/a.wav']);

  session.connectionState = 'connected';
  session.emit(VoiceSessionEvents.voiceStateChanged, {
    sessionId: 'guild-session',
    guildId: 'guild',
    nodeId: null,
    previousState: 'reconnecting',
    state: 'connected',
  });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/b.wav',
  ]);
  assert.equal(
    (await manager.snapshot('guild', 'channel')).current?.url,
    'https://media.example/b.wav',
  );
  await manager.close();
});

test('skip with an empty upcoming queue stops the current track without disconnecting', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/a.wav',
    requestedBy: 'alice',
  });
  assert.equal(await manager.skip('guild', 'channel'), null);
  assert.equal(session.state, 'stopped');
  assert.equal(session.connectionState, 'connected');
  await assert.rejects(manager.skip('guild', 'channel'), /no current track/i);
  await manager.close();
});

test('failed next tracks are skipped and the following valid item starts', async () => {
  const { manager, session } = createManager();
  session.failingUrls.add('https://media.example/unreachable.wav');
  for (const name of ['a', 'unreachable', 'c']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/c.wav');
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/unreachable.wav',
    'https://media.example/c.wav',
  ]);
  await manager.close();
});

test('skip requested during startup waits for the current start and then advances once', async () => {
  const { manager, session } = createManager();
  const barrier = session.holdNextPlay();
  const starting = manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/a.wav',
    requestedBy: 'alice',
  });
  await barrier.started;
  const adding = manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/b.wav',
    requestedBy: 'alice',
  });
  const skipping = manager.skip('guild', 'channel');
  barrier.release();
  await Promise.all([starting, adding]);
  assert.equal((await skipping)?.url, 'https://media.example/b.wav');
  assert.deepEqual(session.playedUrls, [
    'https://media.example/a.wav',
    'https://media.example/b.wav',
  ]);
  assert.equal(session.playedUrls.filter((url) => url.endsWith('/b.wav')).length, 1);
  await manager.close();
});

test('natural completion racing with skip cannot start a track twice or clear its successor', async () => {
  const { manager, session } = createManager();
  for (const name of ['a', 'b', 'c']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  const oldGeneration = session.currentGeneration;
  session.finish('completed', oldGeneration);
  const skipping = manager.skip('guild', 'channel');
  await skipping;
  await new Promise<void>((resolve) => setImmediate(resolve));
  const snapshot = await manager.snapshot('guild', 'channel');
  assert.ok(
    snapshot.current?.url === 'https://media.example/b.wav' ||
      snapshot.current?.url === 'https://media.example/c.wav',
  );
  assert.equal(session.playedUrls.filter((url) => url.endsWith('/b.wav')).length, 1);
  assert.ok(session.playedUrls.filter((url) => url.endsWith('/c.wav')).length <= 1);
  await manager.close();
});

test('stop during automatic queue advancement clears the newly starting track', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/a.wav',
    requestedBy: 'alice',
  });
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/b.wav',
    requestedBy: 'alice',
  });
  const barrier = session.holdNextPlay();
  session.finish('completed');
  await barrier.started;
  const stopping = manager.stop('guild', 'channel');
  barrier.release();
  await stopping;
  const snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.current, null);
  assert.equal(snapshot.queue.length, 0);
  assert.equal(session.connectionState, 'connected');
  await manager.close();
});

test('disconnect during automatic queue advancement disposes the connection after startup settles', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/a.wav',
    requestedBy: 'alice',
  });
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/b.wav',
    requestedBy: 'alice',
  });
  const barrier = session.holdNextPlay();
  session.finish('completed');
  await barrier.started;
  const disconnecting = manager.disconnect('guild', 'channel');
  barrier.release();
  assert.equal(await disconnecting, true);
  assert.equal(session.disposed, true);
  assert.equal(session.connectionState, 'disconnected');
  await manager.close();
});

test('filter and volume state are independent between guild sessions', async () => {
  const sessions = new Map([
    ['guild-a', new FakeDiscordSession('guild-a')],
    ['guild-b', new FakeDiscordSession('guild-b')],
  ]);
  const remedia = {
    createDiscordSession({ guildId }: { guildId: string }) {
      return sessions.get(guildId) as unknown as DiscordSession;
    },
  } as unknown as RemediaClient;
  const manager = new DiscordPlaybackManager(remedia);
  for (const guildId of sessions.keys()) {
    await manager.enqueue(guildId, 'channel', 'bot', {
      url: `https://media.example/${guildId}.wav`,
      requestedBy: 'alice',
    });
  }
  await manager.setFilter('guild-a', 'channel', 'vibrato');
  await manager.setVolume('guild-a', 'channel', 30);
  await manager.setLoopMode('guild-a', 'channel', 'queue');
  assert.deepEqual((await manager.snapshot('guild-a', 'channel')).filterPresets, ['vibrato']);
  assert.equal((await manager.snapshot('guild-a', 'channel')).volumePercent, 30);
  assert.equal((await manager.snapshot('guild-a', 'channel')).loopMode, 'queue');
  assert.deepEqual((await manager.snapshot('guild-b', 'channel')).filterPresets, []);
  assert.equal((await manager.snapshot('guild-b', 'channel')).volumePercent, 100);
  assert.equal((await manager.snapshot('guild-b', 'channel')).loopMode, 'off');
  await manager.close();
});

test('toggling a preset removes only that preset and clears the graph when the list empties', async () => {
  const { manager, session } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/toggle.wav',
    requestedBy: 'alice',
  });
  await manager.setFilters('guild', 'channel', ['bassboost', 'nightcore', 'tremolo']);

  assert.equal(await manager.toggleFilter('guild', 'channel', 'nightcore'), 'removed');
  assert.deepEqual((await manager.snapshot('guild', 'channel')).filterPresets, [
    'bassboost',
    'tremolo',
  ]);
  assert.equal(session.filters.graph, `${FILTER_PRESETS.bassboost},${FILTER_PRESETS.tremolo}`);

  assert.equal(await manager.toggleFilter('guild', 'channel', 'bassboost'), 'removed');
  assert.equal(await manager.toggleFilter('guild', 'channel', 'tremolo'), 'removed');
  assert.deepEqual((await manager.snapshot('guild', 'channel')).filterPresets, []);
  assert.equal(session.filters.graph, null);
  await manager.close();
});

test('bounds the in-memory queue', async () => {
  const { manager } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/current.mp3',
    requestedBy: 'alice',
  });
  for (let index = 0; index < 100; index += 1) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${index}.mp3`,
      requestedBy: 'alice',
    });
  }
  await assert.rejects(
    manager.enqueue('guild', 'channel', 'bot', {
      url: 'https://media.example/overflow.mp3',
      requestedBy: 'alice',
    }),
    /queue is full/i,
  );
  await manager.close();
});

test('rejects control requests from a different voice channel', async () => {
  const { manager } = createManager();
  await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/one.mp3',
    requestedBy: 'alice',
  });

  await assert.rejects(manager.pause('guild', 'other-channel'), /same voice channel/i);
  await manager.close();
});

function createManager(): { manager: DiscordPlaybackManager; session: FakeDiscordSession } {
  const session = new FakeDiscordSession();
  const remedia = {
    createDiscordSession() {
      return session as unknown as DiscordSession;
    },
  } as unknown as RemediaClient;
  return { manager: new DiscordPlaybackManager(remedia), session };
}

async function completeAndSnapshot(
  manager: DiscordPlaybackManager,
  session: FakeDiscordSession,
): Promise<Awaited<ReturnType<DiscordPlaybackManager['snapshot']>>> {
  session.finish('completed');
  await new Promise<void>((resolve) => setImmediate(resolve));
  return manager.snapshot('guild', 'channel');
}

class FakeDiscordSession extends EventEmitter {
  readonly guildId: string;
  readonly channelId = 'channel';
  readonly userId = 'bot';
  readonly playedUrls: string[] = [];
  readonly playMetadata: Array<
    | {
        textChannel?: SendableChannels;
        queueGeneration: number;
        track: { url: string; requestedBy: string };
      }
    | undefined
  > = [];
  connectionState = 'disconnected';
  state: PlaybackState = 'idle';
  position: {
    mediaMs: number | null;
    mediaPositionAccuracy: 'timestamp' | 'estimated' | null;
    submittedMs: number;
    playedMs: number | null;
  } = {
    mediaMs: null,
    mediaPositionAccuracy: null,
    submittedMs: 0,
    playedMs: null,
  };
  durationMs = 10_000;
  error: string | null = null;
  seekPositionMs: number | undefined;
  disposed = false;
  volume = 1;
  readonly filters = {
    graph: null as string | null,
    set: async (graph: string) => {
      const previousGraph = this.filters.graph;
      this.filters.graph = graph;
      this.emit(PlaybackSessionEvents.filterChanged, {
        ...this.#eventIdentity(this.#currentGeneration),
        previousGraph,
        graph,
      });
    },
    clear: async () => {
      const previousGraph = this.filters.graph;
      this.filters.graph = null;
      this.emit(PlaybackSessionEvents.filterChanged, {
        ...this.#eventIdentity(this.#currentGeneration),
        previousGraph,
        graph: null,
      });
    },
  };
  readonly failingUrls = new Set<string>();
  #generation = 0;
  #currentGeneration = 0;
  #nextPlayBarrier:
    | { started: () => void; promise: Promise<void>; release: () => void }
    | undefined;
  readonly #waiters = new Map<number, Array<(state: PlaybackState) => void>>();
  readonly #terminalStates = new Map<number, PlaybackState>();

  constructor(guildId = 'guild') {
    super();
    this.guildId = guildId;
  }

  get currentGeneration(): number {
    return this.#currentGeneration;
  }

  holdNextPlay(): { started: Promise<void>; release(): void } {
    let started!: () => void;
    let release!: () => void;
    const startedPromise = new Promise<void>((resolve) => (started = resolve));
    const gate = new Promise<void>((resolve) => (release = resolve));
    this.#nextPlayBarrier = { started, promise: gate, release };
    return { started: startedPromise, release };
  }

  async connect(): Promise<void> {
    this.connectionState = 'connected';
    this.emit(VoiceSessionEvents.voiceStateChanged, {
      sessionId: `${this.guildId}-session`,
      guildId: this.guildId,
      nodeId: null,
      previousState: 'disconnected',
      state: 'connected',
    });
  }

  async play(
    source: { url: string },
    options?: {
      customMetadata?: {
        textChannel?: SendableChannels;
        queueGeneration: number;
        track: { url: string; requestedBy: string };
      };
    },
  ) {
    this.playedUrls.push(source.url);
    this.playMetadata.push(options?.customMetadata);
    const barrier = this.#nextPlayBarrier;
    this.#nextPlayBarrier = undefined;
    if (barrier) {
      barrier.started();
      await barrier.promise;
    }
    if (this.failingUrls.has(source.url)) {
      this.state = 'failed';
      throw new Error('media source was not found');
    }
    const generation = ++this.#generation;
    this.#currentGeneration = generation;
    this.state = 'playing';
    this.position = {
      mediaMs: null,
      mediaPositionAccuracy: null,
      submittedMs: 0,
      playedMs: null,
    };
    const info = {
      streamIndex: 0,
      codec: 'mp3',
      sampleRate: 48_000,
      channels: 2,
      channelLayout: 'stereo',
      durationMs: 10_000,
      bitRate: null,
      metadata: { title: 'Example title' },
    };
    this.emit(PlaybackSessionEvents.playbackStarted, {
      ...this.#eventIdentity(generation),
      state: 'playing',
      info,
    });
    return info;
  }

  pause(): Promise<void> {
    const previousState = this.state;
    this.state = 'paused';
    this.emit(PlaybackSessionEvents.playbackPaused, {
      ...this.#eventIdentity(this.#currentGeneration),
      previousState,
      state: 'paused',
    });
    return Promise.resolve();
  }

  resume(): Promise<void> {
    const previousState = this.state;
    this.state = 'playing';
    this.emit(PlaybackSessionEvents.playbackResumed, {
      ...this.#eventIdentity(this.#currentGeneration),
      previousState,
      state: 'playing',
    });
    return Promise.resolve();
  }

  seek(positionMs: number): Promise<void> {
    this.seekPositionMs = positionMs;
    this.emit(PlaybackSessionEvents.playbackSeeked, {
      ...this.#eventIdentity(this.#currentGeneration),
      positionMs,
      previousPositionMs: null,
    });
    return Promise.resolve();
  }

  stop(): Promise<void> {
    this.#finish(this.#currentGeneration, 'stopped');
    return Promise.resolve();
  }

  wait(): Promise<PlaybackState> {
    const generation = this.#currentGeneration;
    const terminal = this.#terminalStates.get(generation);
    if (terminal) return Promise.resolve(terminal);
    return new Promise((resolve) => {
      const waiters = this.#waiters.get(generation) ?? [];
      waiters.push(resolve);
      this.#waiters.set(generation, waiters);
    });
  }

  setVolume(gain: number): Promise<void> {
    const previousVolume = this.volume;
    this.volume = gain;
    this.emit(PlaybackSessionEvents.volumeChanged, {
      ...this.#eventIdentity(this.#currentGeneration),
      previousVolume,
      volume: gain,
    });
    return Promise.resolve();
  }

  finish(state: PlaybackState, generation = this.#currentGeneration): void {
    this.#finish(generation, state);
  }

  #finish(generation: number, state: PlaybackState): void {
    this.#terminalStates.set(generation, state);
    if (generation === this.#currentGeneration) this.state = state;
    if (state === 'completed') {
      this.emit(PlaybackSessionEvents.playbackCompleted, {
        ...this.#eventIdentity(generation),
        state,
        position: this.position,
      });
    } else if (state === 'failed') {
      this.emit(PlaybackSessionEvents.playbackFailed, {
        ...this.#eventIdentity(generation),
        state,
        error: new Error('decoder failed'),
      });
    } else if (state === 'stopped') {
      this.emit(PlaybackSessionEvents.playbackStopped, {
        ...this.#eventIdentity(generation),
        state,
      });
    }
    for (const resolve of this.#waiters.get(generation) ?? []) resolve(state);
    this.#waiters.delete(generation);
  }

  async disconnect(): Promise<void> {
    const previousState = this.connectionState;
    this.connectionState = 'disconnected';
    this.emit(VoiceSessionEvents.voiceStateChanged, {
      sessionId: `${this.guildId}-session`,
      guildId: this.guildId,
      nodeId: null,
      previousState,
      state: 'disconnected',
    });
  }

  async dispose(): Promise<void> {
    this.disposed = true;
    await this.disconnect();
  }

  emitVoiceFailure(error: Error): void {
    this.emit(VoiceSessionEvents.voiceFailed, {
      sessionId: `${this.guildId}-session`,
      guildId: this.guildId,
      nodeId: null,
      state: 'failed',
      error,
    });
  }

  #eventIdentity(generation: number): {
    sessionId: string;
    guildId: string;
    nodeId: null;
    generation: number;
    customMetadata?: {
      textChannel?: SendableChannels;
      queueGeneration: number;
      track: { url: string; requestedBy: string };
    };
  } {
    const customMetadata = this.playMetadata[generation - 1];
    return {
      sessionId: `${this.guildId}-session`,
      guildId: this.guildId,
      nodeId: null,
      generation,
      ...(customMetadata ? { customMetadata } : {}),
    };
  }
}

function createTextChannel(): {
  channel: SendableChannels;
  messages: Array<{ embeds?: readonly unknown[] }>;
} {
  const messages: Array<{ embeds?: readonly unknown[] }> = [];
  const channel = {
    async send(message: { embeds?: readonly EmbedBuilder[] }) {
      messages.push(message as { embeds?: readonly unknown[] });
      return {};
    },
  } as unknown as SendableChannels;
  return { channel, messages };
}

function embedTitle(message: { embeds?: readonly unknown[] }): string | undefined {
  return (message.embeds?.[0] as EmbedBuilder | undefined)?.data.title;
}

function embedDescription(message: { embeds?: readonly unknown[] }): string {
  return (message.embeds?.[0] as EmbedBuilder | undefined)?.data.description ?? '';
}
