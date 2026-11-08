import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { test } from 'node:test';
import type {
  AudioStreamInfo,
  DiscordSession,
  DiscordSessionOptions,
  PlaybackState,
  RemediaClient,
  StageNextOptions,
} from '@remedia/client';
import type { EmbedBuilder, SendableChannels } from 'discord.js';
import { DiscordPlaybackManager } from './playback-manager.js';

type Metadata = {
  queueGeneration: number;
  track: { url: string; requestedBy: string };
  textChannel?: SendableChannels;
};
const first = { url: 'https://media.example/first.mp3?signature=private', requestedBy: 'alice' };
const second = { url: 'https://media.example/second.mp3?signature=secret', requestedBy: 'alice' };
const info: AudioStreamInfo = {
  streamIndex: 0,
  codec: 'mp3',
  bitRate: null,
  sampleRate: 48000,
  channels: 2,
  channelLayout: 'stereo',
  durationMs: 90_000,
  metadata: {},
};
const flush = () => new Promise<void>((resolve) => setImmediate(resolve));

for (const mode of ['remote', 'embedded'] as const) {
  test(`${mode}: promotes the second entry once without another play or completion watcher`, async () => {
    const h = harness(mode);
    try {
      await h.start();
      assert.deepEqual(h.options()?.crossfade, {
        enabled: true,
        durationMs: 5000,
        prepareBeforeMs: 30000,
        curve: 'equalPower',
      });
      assert.equal(h.options()?.nodeId, 'test-node');
      assert.equal(h.session.waitCalls, 0);
      h.session.prepare();
      h.session.prepare();
      await flush();
      assert.equal(h.session.stages.length, 1);
      const stage = h.session.stages[0]!;
      assert.equal(stage.options.expectedGeneration, 1);
      assert.equal(stage.options.queueRevision, 1);
      assert.equal(stage.options.sourceRevision, 1);
      assert.ok(stage.options.entryId);
      h.session.promote();
      h.session.promoteEventAgain();
      let snapshot = await h.manager.snapshot('guild', 'channel');
      assert.equal(snapshot.current?.url, second.url);
      assert.equal(snapshot.queue.length, 0);
      h.session.completeFirstWithContinuation();
      h.session.completeFirstWithContinuation();
      await flush();
      assert.deepEqual(h.session.played, [first.url]);
      assert.equal(h.session.waitCalls, 0);
      snapshot = await h.manager.snapshot('guild', 'channel');
      assert.equal(snapshot.current?.url, second.url);
      h.session.finish();
      await flush();
      assert.equal((await h.manager.snapshot('guild', 'channel')).current, null);
      assert.deepEqual(h.titles(), [
        'Crossfade session enabled',
        'Now playing',
        'Preparation requested',
        'Second source staging started',
        'Stage ready',
        'Transition started',
        'Second source promoted',
        'Track ended',
      ]);
      assert.ok(h.descriptions().some((text) => text.includes('remaining 35000 ms')));
      assert.ok(h.descriptions().some((text) => text.includes('type=promoted, generation=2')));
      assert.ok(h.descriptions().every((text) => !text.includes('signature=')));
    } finally {
      await h.manager.close();
    }
  });
}

test('ignored preparation uses ordinary completion to play the queued second URL', async () => {
  const h = harness();
  try {
    await h.start();
    h.session.finish();
    await flush();
    assert.deepEqual(h.session.played, [first.url, second.url]);
    assert.equal(h.session.stages.length, 0);
    assert.equal((await h.manager.snapshot('guild', 'channel')).current?.url, second.url);
    h.session.finish();
    await flush();
    assert.equal((await h.manager.snapshot('guild', 'channel')).current, null);
  } finally {
    await h.manager.close();
  }
});

test('stage errors report a sanitized reason and leave outgoing playback and normal fallback running', async () => {
  const h = harness();
  h.session.stageFailure = new Error(`provider refused ${second.url} password=private`);
  try {
    await h.start();
    h.session.prepare();
    h.session.prepare();
    await flush();
    assert.equal(h.session.stages.length, 1);
    assert.equal(h.session.state, 'playing');
    assert.ok(h.descriptions().some((text) => text.includes('provider refused')));
    assert.ok(
      h.descriptions().every((text) => !text.includes('secret') && !text.includes('private')),
    );
    h.session.finish();
    await flush();
    assert.deepEqual(h.session.played, [first.url, second.url]);
  } finally {
    await h.manager.close();
  }
});

test('seek invalidates preparation and fences old requests while allowing a new timeline request', async () => {
  const h = harness();
  try {
    await h.start();
    h.session.prepare();
    const old = h.session.identity();
    await h.manager.seek('guild', 'channel', 5000);
    h.session.emit('prepareRequested', { ...old, remainingMs: 35000, transitionInMs: 30000 });
    assert.equal(h.session.stages.length, 1);
    h.session.prepare();
    assert.equal(h.session.stages.length, 2);
    assert.equal(h.session.stages[1]?.options.expectedGeneration, 2);
    assert.equal(h.session.stages[1]?.options.sourceRevision, 2);
    assert.equal(h.session.stages[0]?.options.entryId, h.session.stages[1]?.options.entryId);
    assert.ok(h.titles().includes('Stage invalidated'));
  } finally {
    await h.manager.close();
  }
});

for (const action of ['stop', 'skip', 'disconnect'] as const) {
  test(`${action} cancels a pending preparation and stale events cannot restage it`, async () => {
    const h = harness();
    try {
      await h.start();
      h.session.prepare();
      const old = h.session.identity();
      await h.manager[action]('guild', 'channel');
      h.session.emit('prepareRequested', { ...old, remainingMs: 35000, transitionInMs: 30000 });
      assert.equal(h.session.stages.length, 1);
      if (action === 'skip') assert.deepEqual(h.session.played, [first.url, second.url]);
      else assert.deepEqual(h.session.played, [first.url]);
      assert.ok(h.session.stops > 0 || h.session.disposed);
    } finally {
      await h.manager.close();
    }
  });
}

test(
  'a pending staging promise never blocks stop or resurrects stopped playback',
  { timeout: 1000 },
  async () => {
    const h = harness();
    let rejectStage!: (reason: Error) => void;
    h.session.stageBarrier = new Promise((_resolve, reject) => {
      rejectStage = reject;
    });
    try {
      await h.start();
      h.session.prepare();
      await h.manager.stop('guild', 'channel');
      rejectStage(new Error('cancelled'));
      await flush();
      assert.equal((await h.manager.snapshot('guild', 'channel')).current, null);
      assert.deepEqual(h.session.played, [first.url]);
    } finally {
      await h.manager.close();
    }
  },
);

test('interrupted overlap restores outgoing state and falls back once without stale promotion', async () => {
  const h = harness();
  try {
    await h.start();
    h.session.prepare();
    h.session.promote();
    h.session.cancelTransition();
    assert.equal((await h.manager.snapshot('guild', 'channel')).current?.url, first.url);
    h.session.finish();
    await flush();
    assert.deepEqual(h.session.played, [first.url, second.url]);
  } finally {
    await h.manager.close();
  }
});

test('remote disabled or missing capabilities reject before creating or joining a session', async () => {
  const h = harness('remote', false);
  try {
    await assert.rejects(h.start(), /audio.crossfade.enabled.*remedia.config.json/);
    assert.equal(h.options(), undefined);
    assert.equal(h.session.connectionState, 'disconnected');
  } finally {
    await h.manager.close();
  }
});

test('existing playing and idle guild sessions are preserved and require explicit disconnect', async () => {
  const h = harness();
  try {
    await h.manager.enqueue('guild', 'channel', 'bot', first);
    await assert.rejects(h.start(), /disconnect first.*left running/);
    assert.equal(h.session.stops, 0);
    assert.deepEqual(h.session.played, [first.url]);
    await h.manager.stop('guild', 'channel');
    await assert.rejects(h.start(), /disconnect first/);
  } finally {
    await h.manager.close();
  }
});

test('ordinary queues keep completion watchers and default session creation', async () => {
  const h = harness();
  try {
    await h.manager.enqueue('guild', 'channel', 'bot', first);
    await h.manager.enqueue('guild', 'channel', 'bot', second);
    assert.equal(h.options()?.crossfade, undefined);
    assert.equal(h.session.waitCalls, 1);
    h.session.finish();
    await flush();
    assert.deepEqual(h.session.played, [first.url, second.url]);
  } finally {
    await h.manager.close();
  }
});

function harness(mode: 'remote' | 'embedded' = 'remote', available: boolean | null = true) {
  const session = new FakeSession();
  const messages: Array<{ embeds: EmbedBuilder[] }> = [];
  const channel = {
    send: async (message: { embeds: EmbedBuilder[] }) => {
      messages.push(message);
    },
  } as unknown as SendableChannels;
  let options: DiscordSessionOptions | undefined;
  const remedia = {
    nodes: [
      {
        id: 'test-node',
        type: mode,
        status: 'healthy',
        capacity: 10,
        assignedGuilds: [],
        capabilities: available === null ? undefined : { crossfade: { available } },
      },
    ],
    createDiscordSession(value: DiscordSessionOptions) {
      options = value;
      return session as unknown as DiscordSession;
    },
  } as unknown as RemediaClient;
  const manager = new DiscordPlaybackManager(remedia);
  return {
    manager,
    session,
    options: () => options,
    start: () => manager.crossfade('guild', 'channel', 'bot', first, second, channel),
    titles: () => messages.map((message) => message.embeds[0]?.data.title),
    descriptions: () => messages.map((message) => message.embeds[0]?.data.description ?? ''),
  };
}

class FakeSession extends EventEmitter {
  guildId = 'guild';
  channelId = 'channel';
  nodeId = 'test-node';
  connectionState = 'disconnected';
  state: PlaybackState = 'idle';
  position = { mediaMs: 0, submittedMs: 0, playedMs: 0, mediaPositionAccuracy: 'timestamp' };
  durationMs = 90_000;
  streamInfo: AudioStreamInfo = info;
  volume = 1;
  error = null;
  filters = { graph: null, clear: async () => {}, set: async () => {} };
  played: string[] = [];
  stages: Array<{ source: { url: string }; options: StageNextOptions<Metadata> }> = [];
  metadata: Metadata | undefined;
  firstMetadata: Metadata | undefined;
  generation = 0;
  stageFailure: Error | undefined;
  stageBarrier: Promise<never> | undefined;
  stops = 0;
  waitCalls = 0;
  disposed = false;
  #waiters: Array<(state: PlaybackState) => void> = [];
  #overlap = false;
  async connect() {
    this.connectionState = 'connected';
  }
  async play(source: { url: string }, options: { customMetadata: Metadata }) {
    this.played.push(source.url);
    this.metadata = options.customMetadata;
    this.firstMetadata ??= this.metadata;
    this.generation++;
    this.state = 'playing';
    this.emit('playbackStarted', { ...this.identity(), state: 'playing', info: this.streamInfo });
    return this.streamInfo;
  }
  identity() {
    return { generation: this.generation, customMetadata: this.metadata };
  }
  prepare() {
    this.emit('prepareRequested', {
      ...this.identity(),
      remainingMs: 35000,
      transitionInMs: 30000,
    });
  }
  async stageNext(source: { url: string }, options: StageNextOptions<Metadata>) {
    this.stages.push({ source, options });
    if (this.stageBarrier) await this.stageBarrier;
    if (this.stageFailure) throw this.stageFailure;
    this.emit('stageReady', {
      generation: options.expectedGeneration,
      entryId: options.entryId,
      queueRevision: options.queueRevision,
      sourceRevision: options.sourceRevision,
    });
    return {};
  }
  promote() {
    this.#overlap = true;
    this.generation++;
    this.metadata = this.stages.at(-1)!.options.customMetadata;
    this.promoteEventAgain();
  }
  promoteEventAgain() {
    this.emit('playbackStarted', {
      ...this.identity(),
      state: 'playing',
      info,
      transition: {
        type: 'crossfade',
        outgoingGeneration: 1,
        entryId: this.stages.at(-1)!.options.entryId,
      },
    });
  }
  completeFirstWithContinuation() {
    this.#overlap = false;
    this.emit('playbackCompleted', {
      generation: 1,
      customMetadata: this.firstMetadata,
      continuation: {
        type: 'promoted',
        generation: this.generation,
        entryId: this.stages.at(-1)!.options.entryId,
      },
    });
    this.#resolveWaiters();
  }
  async clearStage() {}
  async seek(positionMs: number) {
    const serial = this.generation;
    if (this.#overlap) this.cancelTransition();
    const options = this.stages.at(-1)?.options;
    if (options)
      this.emit('stageInvalidated', { generation: options.expectedGeneration, ...options });
    this.generation = Math.max(serial, this.generation) + 1;
    this.emit('playbackSeeked', { ...this.identity(), positionMs });
  }
  cancelTransition() {
    this.#overlap = false;
    const failedGeneration = this.generation;
    this.generation = 1;
    this.metadata = this.firstMetadata;
    this.emit('transitionCancelled', {
      ...this.identity(),
      failedGeneration,
      entryId: this.stages.at(-1)!.options.entryId,
    });
  }
  finish() {
    this.state = 'completed';
    this.emit('playbackCompleted', this.identity());
    this.#resolveWaiters();
  }
  #resolveWaiters() {
    for (const resolve of this.#waiters.splice(0)) resolve('completed');
  }
  async pause() {
    this.state = 'paused';
    this.emit('playbackPaused', { ...this.identity(), previousState: 'playing', state: 'paused' });
  }
  async resume() {
    this.state = 'playing';
    this.emit('playbackResumed', { ...this.identity(), previousState: 'paused', state: 'playing' });
  }
  async stop() {
    this.stops++;
    this.state = 'stopped';
    this.emit('playbackStopped', this.identity());
    this.#resolveWaiters();
  }
  wait(): Promise<PlaybackState> {
    this.waitCalls++;
    return new Promise((resolve) => this.#waiters.push(resolve));
  }
  async dispose() {
    this.disposed = true;
    await this.stop();
  }
}

test('unknown-duration input reports missing preparation and completes by normal fallback', async () => {
  const h = harness();
  h.session.streamInfo = { ...info, durationMs: null };
  try {
    await h.start();
    assert.ok(h.titles().includes('Preparation unavailable'));
    h.session.finish();
    await flush();
    assert.deepEqual(h.session.played, [first.url, second.url]);
  } finally {
    await h.manager.close();
  }
});

test('legacy remote capability discovery fails before allocating a session', async () => {
  const h = harness('remote', null);
  try {
    await assert.rejects(h.start(), /older servers may not support crossfade/);
    assert.equal(h.options(), undefined);
  } finally {
    await h.manager.close();
  }
});

for (const action of ['stop', 'skip', 'disconnect'] as const) {
  test(`${action} during overlap cannot replay the already audible second source`, async () => {
    const h = harness();
    try {
      await h.start();
      h.session.prepare();
      h.session.promote();
      await h.manager[action]('guild', 'channel');
      h.session.completeFirstWithContinuation();
      await flush();
      assert.deepEqual(h.session.played, [first.url]);
      if (action !== 'disconnect')
        assert.equal((await h.manager.snapshot('guild', 'channel')).current, null);
    } finally {
      await h.manager.close();
    }
  });
}

test('a crossfade session reserves its queue from ordinary play commands', async () => {
  const h = harness();
  try {
    await h.start();
    await assert.rejects(
      h.manager.enqueue('guild', 'channel', 'bot', second),
      /reserved.*disconnect/,
    );
    assert.deepEqual(h.session.played, [first.url]);
    assert.equal((await h.manager.snapshot('guild', 'channel')).queue.length, 1);
  } finally {
    await h.manager.close();
  }
});

test('seek during overlap reports the retained outgoing source and re-prepares on the new generation', async () => {
  const h = harness();
  try {
    await h.start();
    h.session.prepare();
    h.session.promote();
    const result = await h.manager.seekWithContext('guild', 'channel', 1000);
    assert.equal(result.current.url, first.url);
    assert.equal((await h.manager.snapshot('guild', 'channel')).current?.url, first.url);
    h.session.prepare();
    assert.equal(h.session.stages.at(-1)?.options.expectedGeneration, 3);
    assert.deepEqual(h.session.played, [first.url]);
  } finally {
    await h.manager.close();
  }
});

test('pause and resume retain the prepared entry and still allow automatic promotion', async () => {
  const h = harness();
  try {
    await h.start();
    h.session.prepare();
    await h.manager.pause('guild', 'channel');
    assert.equal((await h.manager.snapshot('guild', 'channel')).state, 'paused');
    assert.equal(h.session.stages.length, 1);
    await h.manager.resume('guild', 'channel');
    h.session.promote();
    h.session.completeFirstWithContinuation();
    assert.equal((await h.manager.snapshot('guild', 'channel')).current?.url, second.url);
    assert.deepEqual(h.session.played, [first.url]);
  } finally {
    await h.manager.close();
  }
});

test('clearing the upcoming queue disables preparation and prevents normal completion fallback', async () => {
  const h = harness();
  try {
    await h.start();
    assert.equal(await h.manager.clearQueue('guild', 'channel'), 1);
    h.session.prepare();
    h.session.finish();
    await flush();
    assert.equal(h.session.stages.length, 0);
    assert.deepEqual(h.session.played, [first.url]);
    assert.equal((await h.manager.snapshot('guild', 'channel')).current, null);
  } finally {
    await h.manager.close();
  }
});
