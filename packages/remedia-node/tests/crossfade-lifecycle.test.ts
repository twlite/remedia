import { once } from 'node:events';
import { Readable } from 'node:stream';
import { afterEach, expect, it, vi } from 'vitest';
import { Remedia } from '../lib/index.js';
import { GENERATION_HISTORY_LIMIT, rememberGeneration } from '../lib/playback.js';
import { NativeRemedia } from '../lib/native-binding.js';
import {
  createHeldAudioServer,
  createHeldReadable,
  feedWhilePending,
  makeWave,
  waitFor,
} from './helpers.js';

type HeldReadable = ReturnType<typeof createHeldReadable>;

const cleanups: Array<() => Promise<void> | void> = [];

afterEach(async () => {
  vi.restoreAllMocks();
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

/** Supply the rest of a held source and end its stream so its decoder reaches end-of-file. */
function finish(held: HeldReadable): void {
  let sent: number;
  do {
    sent = held.sendNext(64 * 1024);
  } while (sent > 0);
  held.stream.end();
}

function timeouts(): number {
  return process.getActiveResourcesInfo().filter((name) => name === 'Timeout').length;
}

/**
 * Replace the first batch of native crossfade events that contains `kind` with the queue-overflow
 * marker, as if the bounded queue had evicted them. Everything else is passed through, and native
 * state stays real, so the session must repair itself from the engine.
 */
function loseBatchWith(kind: string): { readonly lost: () => boolean } {
  const original = NativeRemedia.prototype.playbackTakeCrossfadeEvents;
  let lost = false;
  vi.spyOn(NativeRemedia.prototype, 'playbackTakeCrossfadeEvents').mockImplementation(function (
    this: NativeRemedia,
    playbackId: string,
  ) {
    const events = original.call(this, playbackId);
    if (!lost && events.some((event) => event.kind === kind)) {
      lost = true;

      return [{ kind: 'events_dropped', generation: '0', droppedEvents: events.length }];
    }

    return events;
  });

  return { lost: () => lost };
}

it('delivers crossfade events without a polling timer', async () => {
  const intervals = vi.spyOn(globalThis, 'setInterval');
  const engine = await Remedia.create();
  const held = await createHeldAudioServer(makeWave(8));
  cleanups.push(
    () => engine.close(),
    () => held.close(),
  );
  const session = engine.createPlaybackSession<{ track: number }>();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const next = createHeldReadable(makeWave(8));
  const events: string[] = [];
  session.on('prepareRequested', () => events.push('prepare'));
  session.on('stageReady', () => events.push('ready'));
  session.on('playbackStarted', (event) =>
    events.push(event.transition ? 'transition' : 'started'),
  );

  await session.play({ url: held.url }, { customMetadata: { track: 0 } });
  await waitFor(() => (events.includes('prepare') ? true : undefined), 10_000);
  const ready = once(session, 'stageReady');
  await session.stageNext(
    { stream: next.stream },
    { entryId: 'next', queueRevision: 1, customMetadata: { track: 1 } },
  );
  await ready;
  const promoted = once(session, 'playbackCompleted');
  held.finish();
  await promoted;
  finish(next);
  await session.wait();

  expect(events).toEqual(['started', 'prepare', 'ready', 'transition']);
  expect(intervals).not.toHaveBeenCalled();
});

it('keeps notifying after a listener throws', async () => {
  const warnings = vi.spyOn(process, 'emitWarning').mockImplementation(() => undefined);
  const engine = await Remedia.create();
  const origin = await createHeldAudioServer(makeWave(8));
  cleanups.push(
    () => engine.close(),
    () => origin.close(),
  );
  const session = engine.createPlaybackSession();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const next = createHeldReadable(makeWave(8));
  session.on('prepareRequested', () => {
    throw new Error('listener failed');
  });

  await session.play({ url: origin.url });
  await waitFor(() => (warnings.mock.calls.length > 0 ? true : undefined), 10_000);
  const ready = once(session, 'stageReady');
  await session.stageNext({ stream: next.stream }, { entryId: 'next', queueRevision: 1 });
  await ready;
  const promoted = once(session, 'playbackCompleted');
  origin.finish();
  await promoted;
  finish(next);
  await session.wait();

  expect(warnings).toHaveBeenCalledTimes(1);
  expect(warnings.mock.calls[0]?.[1]).toMatchObject({ code: 'REMEDIA_LISTENER_ERROR' });
});

it('promotes a long chain of staged sources and releases every input and timer', async () => {
  const engine = await Remedia.create();
  const origins = await Promise.all(
    Array.from({ length: 6 }, () => createHeldAudioServer(makeWave(8))),
  );
  cleanups.push(() => engine.close(), ...origins.map((origin) => () => origin.close()));
  const session = engine.createPlaybackSession<{ track: number }>();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const last = createHeldReadable(makeWave(8));
  const started: Array<[number, unknown]> = [];
  const completed: Array<[number, unknown]> = [];
  session.on('playbackStarted', (event) => started.push([event.generation, event.customMetadata]));
  session.on('playbackCompleted', (event) =>
    completed.push([event.generation, event.continuation]),
  );

  await session.play({ url: origins[0]!.url }, { customMetadata: { track: 0 } });
  for (let index = 1; index <= origins.length; index++) {
    // Every track but the last is an HTTP source with a known length, so it can fade out; the
    // last is a Readable that is released with the rest of the playback.
    const source = index < origins.length ? { url: origins[index]!.url } : { stream: last.stream };
    const ready = once(session, 'stageReady');
    await session.stageNext(source, {
      entryId: `track-${index}`,
      queueRevision: index,
      customMetadata: { track: index },
    });
    await ready;
    const promoted = once(session, 'playbackCompleted');
    origins[index - 1]!.finish();
    await promoted;
  }
  finish(last);
  await session.wait();
  await waitFor(() => (completed.length === origins.length + 1 ? true : undefined), 10_000);

  const generations = Array.from({ length: origins.length + 1 }, (_unused, index) => index + 1);

  expect(started.map(([generation]) => generation)).toEqual(generations);
  expect(started.map(([, metadata]) => metadata)).toEqual(
    generations.map((_generation, track) => ({ track })),
  );
  expect(completed.map(([generation]) => generation)).toEqual(generations);
  expect(completed.slice(0, -1).map(([, continuation]) => continuation)).toEqual(
    generations.slice(1).map((generation) => ({
      type: 'promoted',
      generation,
      entryId: `track-${generation - 1}`,
    })),
  );
  expect(completed.at(-1)?.[1]).toBeUndefined();
  expect(last.stream.listenerCount('readable')).toBe(0);
  for (const origin of origins) expect(origin.requests).toBe(1);
});

it('repeated playback and disposal cycles leave no timers, loops or sessions behind', async () => {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const wave = makeWave(0.05);
  const timersBefore = timeouts();

  for (let cycle = 0; cycle < 25; cycle++) {
    const session = engine.createPlaybackSession();
    session.configureCrossfade({ enabled: true, durationMs: 1000 });
    await session.play({ buffer: wave });
    await session.wait();
    await session.dispose();
  }

  for (let cycle = 0; cycle < 10; cycle++) {
    // Disposed while the native notification wait is pending and a stage is held.
    const session = engine.createPlaybackSession();
    session.configureCrossfade({ enabled: true, durationMs: 1000 });
    const current = createHeldReadable(makeWave(8));
    const next = createHeldReadable(makeWave(8));
    await session.play({ stream: current.stream });
    await session.stageNext({ stream: next.stream }, { entryId: 'next', queueRevision: 1 });
    await session.dispose();

    expect(current.stream.listenerCount('readable')).toBe(0);
    expect(next.stream.listenerCount('readable')).toBe(0);
  }

  // A leak would add at least one timer per cycle (35 here); unrelated runtime timers add few.
  expect(timeouts()).toBeLessThanOrEqual(timersBefore + 2);
});

it('closes the engine while crossfade sessions wait for native notifications', async () => {
  const engine = await Remedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  cleanups.push(() => held.close());
  const sessions = [engine.createPlaybackSession(), engine.createPlaybackSession()];
  const states: string[] = [];

  for (const session of sessions) {
    session.configureCrossfade({ enabled: true, durationMs: 1000 });
    session.on('playbackStopped', () => states.push('stopped'));
    await session.play({ url: held.url });
  }

  await engine.close();
  await waitFor(() => (states.length === sessions.length ? true : undefined), 5_000);
});

it('adopts a promotion whose events were lost instead of cancelling the live input', async () => {
  const engine = await Remedia.create();
  const origin = await createHeldAudioServer(makeWave(8));
  cleanups.push(
    () => engine.close(),
    () => origin.close(),
  );
  const session = engine.createPlaybackSession<{ track: string }>();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const second = createHeldReadable(makeWave(20));
  const completed: Array<{ generation: number; metadata: unknown }> = [];
  const failed: unknown[] = [];
  const invalidated: unknown[] = [];
  session.on('playbackCompleted', (event) =>
    completed.push({ generation: event.generation, metadata: event.customMetadata }),
  );
  session.on('playbackFailed', (event) => failed.push(event));
  session.on('stageInvalidated', (event) => invalidated.push(event));

  await session.play({ url: origin.url }, { customMetadata: { track: 'first' } });
  const ready = once(session, 'stageReady');
  await session.stageNext(
    { stream: second.stream },
    { entryId: 'second', queueRevision: 1, customMetadata: { track: 'second' } },
  );
  await ready;
  const loss = loseBatchWith('transition_started');
  origin.finish();
  await waitFor(() => (loss.lost() ? true : undefined), 5_000);
  await waitFor(() => (session.currentGeneration === 2 ? true : undefined), 5_000);

  // The promotion's events are gone, but the session now plays the second stream. Its input must
  // not be mistaken for a dead stage and released after the fade-sized delay a stale stage gets.
  await new Promise((resolve) => setTimeout(resolve, 3_500));

  expect(session.state).toBe('playing');
  expect(second.stream.listenerCount('readable')).toBeGreaterThan(0);

  finish(second);
  await session.wait();
  await waitFor(
    () => (completed.some((event) => event.generation === 2) ? true : undefined),
    10_000,
  );

  // The outgoing track's completion may have been lost with the batch or may arrive afterwards;
  // either is legitimate. The final track completes exactly once, with the adopted metadata.
  expect(completed.map((event) => event.generation)).toSatisfy((generations: number[]) =>
    [[2], [1, 2]].some((expected) => JSON.stringify(expected) === JSON.stringify(generations)),
  );
  expect(failed).toEqual([]);
  expect(invalidated).toEqual([]);
  expect(completed.at(-1)).toEqual({ generation: 2, metadata: { track: 'second' } });
  expect(second.stream.listenerCount('readable')).toBe(0);
}, 30_000);

it('carries metadata across a timeline change whose events were lost', async () => {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession<{ track: string }>();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const source = createHeldReadable(makeWave(20));
  const stopped: Array<{ generation: number; metadata: unknown }> = [];
  session.on('playbackStopped', (event) =>
    stopped.push({ generation: event.generation, metadata: event.customMetadata }),
  );

  await session.play({ stream: source.stream }, { customMetadata: { track: 'only' } });
  const loss = loseBatchWith('timeline_changed');
  await feedWhilePending(session.seek(8_000), source, 'seek with lost events');

  expect(loss.lost()).toBe(true);
  expect(session.currentGeneration).toBe(2);

  await session.stop();

  expect(stopped).toEqual([{ generation: 2, metadata: { track: 'only' } }]);
});

it('reports a stage whose invalidation was lost and releases its input', async () => {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const source = createHeldReadable(makeWave(20));
  const next = createHeldReadable(makeWave(20));
  const invalidated: Array<{ entryId: string; queueRevision: number }> = [];
  session.on('stageInvalidated', (event) =>
    invalidated.push({ entryId: event.entryId, queueRevision: event.queueRevision }),
  );

  await session.play({ stream: source.stream });
  const ready = once(session, 'stageReady');
  await session.stageNext({ stream: next.stream }, { entryId: 'next', queueRevision: 7 });
  await ready;
  const loss = loseBatchWith('stage_invalidated');
  await feedWhilePending(session.seek(8_000), source, 'seek that invalidates the stage');

  expect(loss.lost()).toBe(true);
  expect(invalidated).toEqual([{ entryId: 'next', queueRevision: 7 }]);
  expect(session.stagedSource).toBeNull();
  // The input may still be needed for a fade of unknown progress, so it is released shortly
  // after the fade length rather than at once.
  await waitFor(() => (next.stream.listenerCount('readable') === 0 ? true : undefined), 6_000);

  await session.stop();
});

it('keeps lifecycle deduplication correct across many generation changes', async () => {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const source = createHeldReadable(makeWave(40));
  const stopped: number[] = [];
  session.on('playbackStopped', (event) => stopped.push(event.generation));

  await session.play({ stream: source.stream });
  for (let seek = 1; seek <= GENERATION_HISTORY_LIMIT + 4; seek++) {
    // Streams can only seek forward, so each target is ahead of what was decoded so far.
    await feedWhilePending(session.seek(6_000 + seek * 1_000), source, `seek ${seek}`);
  }

  expect(session.currentGeneration).toBe(GENERATION_HISTORY_LIMIT + 5);

  await session.stop();
  await session.wait();

  expect(stopped).toEqual([GENERATION_HISTORY_LIMIT + 5]);
});

it('bounds the remembered generations to the most recent ones', () => {
  const generations = new Set<number>();

  for (let generation = 1; generation <= GENERATION_HISTORY_LIMIT * 4; generation++) {
    rememberGeneration(generations, generation);
    expect(generations.size).toBeLessThanOrEqual(GENERATION_HISTORY_LIMIT);
  }

  expect([...generations]).toEqual(
    Array.from(
      { length: GENERATION_HISTORY_LIMIT },
      (_unused, index) => GENERATION_HISTORY_LIMIT * 3 + index + 1,
    ),
  );

  // Remembering an existing generation refreshes it instead of growing the set.
  rememberGeneration(generations, GENERATION_HISTORY_LIMIT * 3 + 1);
  expect([...generations].at(-1)).toBe(GENERATION_HISTORY_LIMIT * 3 + 1);
  expect(generations.size).toBe(GENERATION_HISTORY_LIMIT);
});

it('plays plain Readable input with crossfade enabled and no stage', async () => {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession();
  session.configureCrossfade({ enabled: true, durationMs: 1000 });
  const input = Readable.from([makeWave(0.2)]);
  const completed = once(session, 'playbackCompleted');

  await session.play({ stream: input });
  await completed;
  await session.wait();

  expect(input.listenerCount('readable')).toBe(0);
});
