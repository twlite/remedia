import { Readable } from 'node:stream';
import { afterEach, expect, it, vi } from 'vitest';
import { Remedia } from '../lib/index.js';
import { NativeRemedia } from '../lib/native-binding.js';
import { createHeldReadable, makeWave, waitFor } from './helpers.js';

const cleanups: Array<() => Promise<void> | void> = [];

afterEach(async () => {
  vi.restoreAllMocks();
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

function deferred(): { promise: Promise<void>; resolve(): void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => (resolve = done));

  return { promise, resolve };
}

/** Let every continuation that is already queued, and the cleanup it triggers, run to the end. */
async function settle(): Promise<void> {
  for (let turn = 0; turn < 20; turn++) await new Promise((resolve) => setImmediate(resolve));
}

/**
 * Hold the nth native `wait` call open and then resolve it with `result`, as if the worker's
 * terminal notification reached JavaScript late. Native state is untouched, so everything else
 * behaves as in production. Call 1 is the lifecycle observer that `play()` starts.
 */
function delayWait(call: number, result: 'completed' | 'stopped'): { release(): void } {
  const gate = deferred();
  const original = NativeRemedia.prototype.wait;
  let calls = 0;
  vi.spyOn(NativeRemedia.prototype, 'wait').mockImplementation(async function (
    this: NativeRemedia,
    playbackId: string,
    generation: string,
  ) {
    if (++calls !== call) return original.call(this, playbackId, generation);
    await gate.promise;

    return result;
  });

  return { release: () => gate.resolve() };
}

async function createSession() {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession();
  const events = {
    completed: [] as number[],
    stopped: [] as number[],
    failed: [] as number[],
  };
  session.on('playbackCompleted', (event) => events.completed.push(event.generation));
  session.on('playbackStopped', (event) => events.stopped.push(event.generation));
  session.on('playbackFailed', (event) => events.failed.push(event.generation));

  return { session, events };
}

it("a late observer of a stopped playback leaves the replacement's input and events alone", async () => {
  const delayed = delayWait(1, 'stopped');
  const { session, events } = await createSession();
  const first = createHeldReadable(makeWave(8));
  const second = createHeldReadable(makeWave(8));

  await session.play({ stream: first.stream });
  await session.stop();
  await session.play({ stream: second.stream });

  // The first playback's observer now learns that its worker stopped.
  delayed.release();
  await settle();

  expect(session.state).toBe('playing');
  expect(second.stream.listenerCount('readable')).toBeGreaterThan(0);
  expect(first.stream.listenerCount('readable')).toBe(0);
  expect(events.failed).toEqual([]);
  // The stop already reported generation 1, so a late duplicate must not surface as generation 2.
  expect(events.stopped).toEqual([1]);

  await session.stop();
  expect(events.stopped).toEqual([1, 2]);
  expect(second.stream.listenerCount('readable')).toBe(0);
});

it('reports a late natural completion for the playback that completed, not its replacement', async () => {
  const delayed = delayWait(1, 'completed');
  const { session, events } = await createSession();
  const second = createHeldReadable(makeWave(8));

  await session.play({ buffer: makeWave(0.1) });
  await waitFor(() => (session.state === 'completed' ? true : undefined));
  await session.play({ stream: second.stream });

  delayed.release();
  await settle();

  expect(events.completed).toEqual([1]);
  expect(session.state).toBe('playing');
  expect(second.stream.listenerCount('readable')).toBeGreaterThan(0);

  await session.stop();
  expect(events.stopped).toEqual([2]);
});

it("a late wait() of a replaced playback does not release the replacement's input", async () => {
  // Call 1 is the lifecycle observer, call 2 is the application's wait().
  const delayed = delayWait(2, 'stopped');
  const { session, events } = await createSession();
  const first = createHeldReadable(makeWave(8));
  const second = createHeldReadable(makeWave(8));

  await session.play({ stream: first.stream });
  const waiting = session.wait();
  await session.stop();
  await session.play({ stream: second.stream });

  delayed.release();
  await expect(waiting).resolves.toBe('stopped');
  await settle();

  expect(session.state).toBe('playing');
  expect(second.stream.listenerCount('readable')).toBeGreaterThan(0);
  expect(events.failed).toEqual([]);

  await session.stop();
});

it("a stop() that overlaps a replacement does not release the replacement's input", async () => {
  const gate = deferred();
  const original = NativeRemedia.prototype.stop;
  let stops = 0;
  vi.spyOn(NativeRemedia.prototype, 'stop').mockImplementation(async function (
    this: NativeRemedia,
    playbackId: string,
  ) {
    const result = await original.call(this, playbackId);
    if (++stops === 1) await gate.promise;

    return result;
  });
  const { session, events } = await createSession();
  const first = createHeldReadable(makeWave(8));
  const second = createHeldReadable(makeWave(8));

  await session.play({ stream: first.stream });
  const stopping = session.stop();
  // The engine has stopped, but this stop() call has not yet seen it.
  await waitFor(() => (session.state === 'stopped' ? true : undefined));
  await session.play({ stream: second.stream });

  gate.resolve();
  await stopping;
  await settle();

  expect(session.state).toBe('playing');
  expect(second.stream.listenerCount('readable')).toBeGreaterThan(0);
  expect(first.stream.listenerCount('readable')).toBe(0);
  // Whoever reports the first playback's end reports generation 1, never the replacement's.
  expect(events.stopped).toEqual([1]);
  expect(events.failed).toEqual([]);

  await session.stop();
});

it("a late observer does not detach the replacement's abort signal", async () => {
  const delayed = delayWait(1, 'stopped');
  const { session, events } = await createSession();
  const first = createHeldReadable(makeWave(8));
  const second = createHeldReadable(makeWave(8));
  const controller = new AbortController();

  await session.play({ stream: first.stream });
  await session.stop();
  await session.play({ stream: second.stream }, { signal: controller.signal });

  delayed.release();
  await settle();
  controller.abort();

  await waitFor(() => (session.state === 'stopped' ? true : undefined));
  expect(events.stopped).toContain(2);
});

it("aborting the previous playback's signal cannot stop its replacement", async () => {
  const delayed = delayWait(1, 'completed');
  const { session, events } = await createSession();
  const second = createHeldReadable(makeWave(8));
  const previous = new AbortController();

  await session.play({ buffer: makeWave(0.1) }, { signal: previous.signal });
  await waitFor(() => (session.state === 'completed' ? true : undefined));
  // The first playback ended, but its observer has not run yet, so its abort binding is still set.
  await session.play({ stream: second.stream });

  previous.abort();
  delayed.release();
  await settle();

  expect(session.state).toBe('playing');
  expect(second.stream.listenerCount('readable')).toBeGreaterThan(0);
  expect(events.stopped).toEqual([]);
  expect(events.completed).toEqual([1]);

  await session.stop();
});

it('survives rapid stop and replace cycles without losing the active input', async () => {
  const { session, events } = await createSession();
  let active = createHeldReadable(makeWave(8));

  await session.play({ stream: active.stream });

  for (let cycle = 0; cycle < 20; cycle++) {
    const next = createHeldReadable(makeWave(8));
    const previous = active;
    await session.stop();
    await session.play({ stream: next.stream });
    active = next;
    await settle();

    expect(session.state).toBe('playing');
    expect(active.stream.listenerCount('readable')).toBeGreaterThan(0);
    expect(previous.stream.listenerCount('readable')).toBe(0);
  }

  expect(events.failed).toEqual([]);
  expect(events.stopped).toEqual(Array.from({ length: 20 }, (_unused, index) => index + 1));

  await session.stop();
  expect(active.stream.listenerCount('readable')).toBe(0);
});

it('still releases the input of the current playback when its observer finishes', async () => {
  const { session, events } = await createSession();
  const input = Readable.from([makeWave(0.3)]);

  await session.play({ stream: input });

  await expect(session.wait()).resolves.toBe('completed');
  await waitFor(() => (events.completed.length === 1 ? true : undefined));

  expect(events.completed).toEqual([1]);
  expect(input.listenerCount('readable')).toBe(0);
});
