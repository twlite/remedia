import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { DiscordSession, PlaybackState, Remedia } from '@remedia/node';
import { DiscordPlaybackManager } from './playback-manager.js';

test('queues tracks, advances on completion, and leaves voice connected when stopping', async () => {
  const { manager, session } = createManager();

  const first = await manager.enqueue('guild', 'channel', 'bot', {
    url: 'https://media.example/one.mp3',
    requestedBy: 'alice',
  });
  assert.equal(first.started, true);
  assert.deepEqual(session.playedUrls, ['https://media.example/one.mp3']);

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
  } as unknown as Remedia;
  return { manager: new DiscordPlaybackManager(remedia), session };
}

class FakeDiscordSession {
  readonly guildId = 'guild';
  readonly channelId = 'channel';
  readonly userId = 'bot';
  readonly playedUrls: string[] = [];
  connectionState = 'disconnected';
  state: PlaybackState = 'idle';
  position = { submittedMs: 0, playedMs: null };
  durationMs = 10_000;
  error: string | null = null;
  seekPositionMs: number | undefined;
  disposed = false;
  #finishCurrent: ((state: PlaybackState) => void) | undefined;

  async connect(): Promise<void> {
    this.connectionState = 'connected';
  }

  async play(source: { url: string }) {
    this.playedUrls.push(source.url);
    this.state = 'playing';
    this.position = { submittedMs: 0, playedMs: null };
    this.#finishCurrent = undefined;
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
    this.state = 'stopped';
    this.#finishCurrent?.('stopped');
    return Promise.resolve();
  }

  wait(): Promise<PlaybackState> {
    return new Promise((resolve) => {
      this.#finishCurrent = (state) => {
        this.state = state;
        resolve(state);
      };
    });
  }

  finish(state: PlaybackState): void {
    this.#finishCurrent?.(state);
  }

  async disconnect(): Promise<void> {
    this.connectionState = 'disconnected';
  }

  async dispose(): Promise<void> {
    this.disposed = true;
    await this.disconnect();
  }
}
