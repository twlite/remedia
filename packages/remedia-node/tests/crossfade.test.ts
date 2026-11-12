import { once } from 'node:events';
import { Readable } from 'node:stream';
import { expect, it } from 'vitest';
import { Remedia } from '../lib/index.js';
import { createHeldAudioServer, makeWave } from './helpers.js';

it.each(['equalPower', 'adaptive'] as const)(
  'promotes a staged Readable once, preserves metadata and emits continuation completion (%s)',
  async (curve) => {
    const engine = await Remedia.create();
    const held = await createHeldAudioServer(makeWave(20), 512 * 1024);
    const session = engine.createPlaybackSession<{ entry: string }>();
    // A one-second fade fits entirely in the ready buffer, so the no-op output
    // can run at full speed without depending on decoder thread scheduling.
    session.configureCrossfade({ enabled: true, durationMs: 1000, curve });
    const events: Array<{
      kind: string;
      generation: number;
      continuation?: unknown;
      customMetadata?: unknown;
    }> = [];
    session.on('playbackStarted', (e) => events.push({ kind: 'started', ...e }));
    session.on('playbackCompleted', (e) => events.push({ kind: 'completed', ...e }));
    try {
      await session.play({ url: held.url }, { customMetadata: { entry: 'first' } });
      const input = Readable.from([makeWave(10)]);
      const ready = once(session, 'stageReady');
      await session.stageNext(
        { stream: input },
        { entryId: 'second', queueRevision: 1, customMetadata: { entry: 'second' } },
      );
      await ready;
      const completion = once(session, 'playbackCompleted');
      held.finish();
      await completion;
      await session.wait();
      await expect.poll(() => events.length).toBe(4);
      expect(events.map((e) => [e.kind, e.generation])).toEqual([
        ['started', 1],
        ['started', 2],
        ['completed', 1],
        ['completed', 2],
      ]);
      expect(events[2]?.continuation).toEqual({
        type: 'promoted',
        generation: 2,
        entryId: 'second',
      });
      expect(events[1]?.customMetadata).toEqual({ entry: 'second' });
      expect(held.requests).toBe(1);
      expect(input.listenerCount('readable')).toBe(0);
    } finally {
      await engine.close();
      await held.close();
    }
  },
  15_000,
);

it('defaults to equalPower and rejects unsupported curves', async () => {
  const engine = await Remedia.create();
  try {
    const session = engine.createPlaybackSession();
    expect(() => session.configureCrossfade({ enabled: true, curve: 'linear' as never })).toThrow(
      /equalPower or adaptive/,
    );
    expect(() => session.configureCrossfade({ enabled: true, curve: 'adaptive' })).not.toThrow();
    expect(() => session.configureCrossfade({ enabled: true })).not.toThrow();
  } finally {
    await engine.close();
  }
});

it('keeps crossfade disabled until explicitly configured', async () => {
  const engine = await Remedia.create();
  try {
    const session = engine.createPlaybackSession();
    await expect(
      session.stageNext({ buffer: makeWave(1) }, { entryId: 'next', queueRevision: 1 }),
    ).rejects.toThrow(/disabled/);
  } finally {
    await engine.close();
  }
});

it('replacing a repeated entry fences invalidation by both revisions and releases its stream', async () => {
  const engine = await Remedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  const session = engine.createPlaybackSession();
  session.configureCrossfade({ enabled: true });
  const first = Readable.from([makeWave(10)]);
  const second = Readable.from([makeWave(10)]);
  try {
    await session.play({ url: held.url });
    await session.stageNext({ stream: first }, { entryId: 'repeat', queueRevision: 1 });
    await session.stageNext({ stream: second }, { entryId: 'repeat', queueRevision: 2 });
    await expect.poll(() => session.stagedSource?.state).toBe('ready');
    expect(session.stagedSource?.queueRevision).toBe(2);
    await expect(
      session.stageNext({ buffer: makeWave(10) }, { entryId: 'repeat', queueRevision: 1 }),
    ).rejects.toThrow();
    expect(session.stagedSource?.queueRevision).toBe(2);
    await session.clearStage();
    expect(session.stagedSource).toBeNull();
    expect(first.listenerCount('readable')).toBe(0);
    expect(second.listenerCount('readable')).toBe(0);
  } finally {
    await engine.close();
    await held.close();
  }
});
