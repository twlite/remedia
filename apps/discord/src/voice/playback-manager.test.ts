import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { DiscordSession, PlaybackState, RemediaClient } from '@remedia/client';
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

test('queues tracks, advances on completion, and leaves voice connected when stopping', async () => {
  const { manager, session } = createManager();

  const first = await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/one.mp3',
    requestedBy: 'alice',
  });
  assert.equal(first.started, true);
  assert.deepEqual(session.playedUrls, ['https://media.example/one.mp3']);
  await manager.setFilter('guild', 'channel', 'bassboost');
  await manager.setVolume('guild', 'channel', 75);

  const second = await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/two.mp3',
    requestedBy: 'bob',
  });
  assert.equal(second.started, false);
  assert.equal(second.queuePosition, 1);

  await manager.pause('guild', 'channel');
  assert.equal(session.state, 'paused');
  await manager.resume('guild', 'channel');
  await manager.seek('guild', 'channel', 12_500);
  assert.equal(session.seekPositionMs, 12_500);

  session.finish('completed');
  await Promise.resolve();
  const snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.current?.url, 'https://media.example/two.mp3');
  assert.deepEqual(session.playedUrls, [
    'https://media.example/one.mp3',
    'https://media.example/two.mp3',
  ]);
  assert.equal(session.filters.graph, FILTER_PRESETS.bassboost);
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

test('consecutive skips start every queued track exactly once and preserve per-guild effects', async () => {
  const { manager, session } = createManager();
  for (const name of ['a', 'b', 'c']) {
    await manager.enqueue('guild', 'channel', 'bot', {
      url: `https://media.example/${name}.wav`,
      requestedBy: 'alice',
    });
  }
  await manager.setFilter('guild', 'channel', 'nightcore');
  await manager.setVolume('guild', 'channel', 150);

  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/b.wav');
  assert.equal((await manager.skip('guild', 'channel'))?.url, 'https://media.example/c.wav');
  assert.equal(session.playedUrls.filter((url) => url.endsWith('/b.wav')).length, 1);
  assert.equal(session.playedUrls.filter((url) => url.endsWith('/c.wav')).length, 1);
  assert.equal(session.filters.graph, 'asetrate=60000,aresample=48000');
  assert.equal(session.volume, 1.5);
  session.finish('completed');
  await Promise.resolve();
  const snapshot = await manager.snapshot('guild', 'channel');
  assert.equal(snapshot.current, null);
  assert.equal(snapshot.queue.length, 0);
  assert.equal(snapshot.connectionState, 'connected');
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
  assert.equal((await manager.snapshot('guild-a', 'channel')).filterPreset, 'vibrato');
  assert.equal((await manager.snapshot('guild-a', 'channel')).volumePercent, 30);
  assert.equal((await manager.snapshot('guild-b', 'channel')).filterPreset, null);
  assert.equal((await manager.snapshot('guild-b', 'channel')).volumePercent, 100);
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

class FakeDiscordSession {
  readonly guildId: string;
  readonly channelId = 'channel';
  readonly userId = 'bot';
  readonly playedUrls: string[] = [];
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
      this.filters.graph = graph;
    },
    clear: async () => {
      this.filters.graph = null;
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
  }

  async play(source: { url: string }) {
    this.playedUrls.push(source.url);
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
    return {
      streamIndex: 0,
      codec: 'mp3',
      sampleRate: 48_000,
      channels: 2,
      channelLayout: 'stereo',
      durationMs: 10_000,
      bitRate: null,
      metadata: {},
    };
  }

  pause(): Promise<void> {
    this.state = 'paused';
    return Promise.resolve();
  }

  resume(): Promise<void> {
    this.state = 'playing';
    return Promise.resolve();
  }

  seek(positionMs: number): Promise<void> {
    this.seekPositionMs = positionMs;
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
    this.volume = gain;
    return Promise.resolve();
  }

  finish(state: PlaybackState, generation = this.#currentGeneration): void {
    this.#finish(generation, state);
  }

  #finish(generation: number, state: PlaybackState): void {
    this.#terminalStates.set(generation, state);
    if (generation === this.#currentGeneration) this.state = state;
    for (const resolve of this.#waiters.get(generation) ?? []) resolve(state);
    this.#waiters.delete(generation);
  }

  async disconnect(): Promise<void> {
    this.connectionState = 'disconnected';
  }

  async dispose(): Promise<void> {
    this.disposed = true;
    await this.disconnect();
  }
}
