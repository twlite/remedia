import { execFileSync } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { afterEach, expect, it } from 'vitest';
import { NativeRemedia } from '../lib/native-binding.js';
import { createHeldAudioServer, makeWave } from './helpers.js';

const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

/** Count this process's OS threads, or null where the platform offers no cheap way to do so. */
function countThreads(): number | null {
  if (process.platform === 'linux') return readdirSync('/proc/self/task').length;
  if (process.platform === 'darwin') {
    const rows = execFileSync('ps', ['-M', '-p', String(process.pid)], { encoding: 'utf8' });
    return rows.trim().split('\n').length - 1;
  }

  return null;
}

async function settleWithin<T>(
  promise: Promise<T>,
  milliseconds: number,
  what: string,
): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new Error(`${what} did not settle in ${milliseconds} ms`)),
          milliseconds,
        );
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function startHeldPlayback(native: NativeRemedia, url: string): Promise<string> {
  const id = native.createPlaybackSession();
  native.playbackConfigureCrossfade(id, true, 1000, 30_000);
  await native.play(id, { kind: 'url', value: url });

  return id;
}

it('wakes a pending event wait when the playback stops and resolves stale revisions at once', async () => {
  const native = await NativeRemedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  cleanups.push(
    () => native.close(),
    () => held.close(),
  );
  const id = await startHeldPlayback(native, held.url);
  const revision = native.playbackEventsRevision(id);
  let settled = false;
  const waiting = native.playbackWaitEvents(id, revision).then((next) => {
    settled = true;
    return next;
  });

  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(settled).toBe(false);

  await native.stop(id);
  const next = await settleWithin(waiting, 2_000, 'the event wait');

  expect(next).not.toBe(revision);
  expect(native.playbackState(id)).toBe('stopped');
  await expect(
    settleWithin(native.playbackWaitEvents(id, revision), 2_000, 'a stale event wait'),
  ).resolves.toBe(native.playbackEventsRevision(id));
});

it('rejects event waits for unknown playbacks and malformed revisions', async () => {
  const native = await NativeRemedia.create();
  cleanups.push(() => native.close());
  const id = native.createPlaybackSession();

  await expect(native.playbackWaitEvents('does-not-exist', '0')).rejects.toThrow(/unknown/);
  await expect(native.playbackWaitEvents(id, 'not-a-number')).rejects.toThrow(/revision/);
  expect(() => native.playbackEventsRevision('does-not-exist')).toThrow(/unknown/);
});

it('wakes every waiter of a playback and keeps independent playbacks independent', async () => {
  const native = await NativeRemedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  cleanups.push(
    () => native.close(),
    () => held.close(),
  );
  const first = await startHeldPlayback(native, held.url);
  const second = await startHeldPlayback(native, held.url);
  const secondRevision = native.playbackEventsRevision(second);
  const results = {
    first: [] as string[],
    second: false,
  };
  const firstRevision = native.playbackEventsRevision(first);
  const firstWaits = [0, 1, 2].map(() =>
    native.playbackWaitEvents(first, firstRevision).then((next) => results.first.push(next)),
  );
  const secondWait = native.playbackWaitEvents(second, secondRevision).then(() => {
    results.second = true;
  });

  await native.stop(first);
  await settleWithin(Promise.all(firstWaits), 2_000, 'the first playback waits');

  expect(results.first).toHaveLength(3);
  expect(results.second).toBe(false);

  await native.stop(second);
  await settleWithin(secondWait, 2_000, 'the second playback wait');
  expect(results.second).toBe(true);
});

it('holds no blocking-pool thread for a pending completion wait', async () => {
  const baseline = countThreads();
  if (baseline === null) return;
  const native = await NativeRemedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  cleanups.push(
    () => native.close(),
    () => held.close(),
  );
  const ids: string[] = [];

  // Start sequentially so the blocking pool stays small; the playbacks' own decoder threads are
  // then the only per-playback threads.
  for (let index = 0; index < 24; index++) ids.push(await startHeldPlayback(native, held.url));
  await new Promise((resolve) => setTimeout(resolve, 200));
  const playing = countThreads() ?? baseline;
  const waits = ids.map((id) => native.wait(id, native.playbackGeneration(id)));

  await new Promise((resolve) => setTimeout(resolve, 300));
  const waiting = countThreads() ?? baseline;

  // A blocking wait would add one thread per pending playback (24 here).
  expect(waiting - playing).toBeLessThan(6);

  await Promise.all(ids.map((id) => native.stop(id)));
  const states = await settleWithin(Promise.all(waits), 5_000, 'the completion waits');

  expect(states).toEqual(ids.map(() => 'stopped'));
});

it('closes promptly while completion and event waits are pending', async () => {
  const native = await NativeRemedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  cleanups.push(() => held.close());
  const ids = [
    await startHeldPlayback(native, held.url),
    await startHeldPlayback(native, held.url),
  ];
  // A wait may be rejected as closed if close() wins the race to its first check, so its outcome
  // is collected from the moment it exists instead of after close() settles.
  const outcomesPending = Promise.allSettled(
    ids.flatMap((id) => [
      native.wait(id, native.playbackGeneration(id)),
      native.playbackWaitEvents(id, native.playbackEventsRevision(id)),
    ]),
  );

  await settleWithin(native.close(), 5_000, 'close');
  const outcomes = await settleWithin(outcomesPending, 5_000, 'the pending waits');

  expect(outcomes).toHaveLength(4);
});

it('disposing a playback releases its pending waits', async () => {
  const native = await NativeRemedia.create();
  const held = await createHeldAudioServer(makeWave(20));
  cleanups.push(
    () => native.close(),
    () => held.close(),
  );
  const id = await startHeldPlayback(native, held.url);
  const completion = native.wait(id, native.playbackGeneration(id));
  const events = native.playbackWaitEvents(id, native.playbackEventsRevision(id));
  // Both waits look the playback up when they first run. Give them time to register, so this
  // exercises releasing pending waits rather than racing disposal against their start.
  const outcomes = Promise.allSettled([completion, events]);
  await new Promise((resolve) => setTimeout(resolve, 100));

  await native.disposePlaybackSession(id);

  await expect(settleWithin(completion, 2_000, 'the completion wait')).resolves.toBe('stopped');
  await settleWithin(events, 2_000, 'the event wait');
  await outcomes;
});
