import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Readable } from 'node:stream';
import { afterAll, beforeAll, test } from 'vitest';

import { Remedia } from '../lib/index.js';
import { createWaveFixture, makeWave, waitFor } from './helpers.js';

let fixture: Awaited<ReturnType<typeof createWaveFixture>>;

beforeAll(async () => {
  fixture = await createWaveFixture();
});

afterAll(async () => {
  await fixture?.cleanup();
});

test('plays a local file and reports metadata and completion', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const info = await playback.play({ file: fixture.path });
    assert.equal(info.sampleRate, 48_000);
    assert.equal(info.channels, 1);
    assert.equal(await playback.wait(), 'completed');
    assert.ok(playback.position.submittedMs > 0);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('decodes Buffer and Uint8Array sources using native-owned byte storage', async () => {
  const encoded = await readFile(fixture.path);
  const padded = Buffer.concat([Buffer.from('prefix'), encoded, Buffer.from('suffix')]);
  const view = new Uint8Array(padded.buffer, padded.byteOffset + 6, encoded.length);
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const info = await playback.play({ buffer: view });
    assert.equal(info.sampleRate, 48_000);
    assert.equal(await playback.wait(), 'completed');
    assert.ok(playback.position.submittedMs > 0);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('decodes compressed MP3 bytes without a file or network intermediary', async () => {
  const compressed = await readFile(
    new URL('../../../crates/remedia-core/tests/fixtures/generated-tone.mp3', import.meta.url),
  );
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const info = await playback.play({ buffer: compressed });
    assert.ok(info.codec.startsWith('mp3'));
    assert.equal(await playback.wait(), 'completed');
    assert.ok(playback.position.submittedMs > 0);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('feeds Readable input in irregular chunks and completes on clean EOF', async () => {
  const encoded = await readFile(fixture.path);
  const sizes = [1, 17, 3, 64, 9, 127, 5, 31];
  const source = Readable.from(
    (async function* () {
      let offset = 0;
      let index = 0;
      while (offset < encoded.length) {
        const size = sizes[index % sizes.length] ?? 1;
        index++;
        const end = Math.min(offset + size, encoded.length);
        yield encoded.subarray(offset, end);
        offset = end;
      }
    })(),
    { objectMode: false, highWaterMark: 64 },
  );
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const info = await playback.play({ stream: source });
    assert.equal(info.sampleRate, 48_000);
    assert.equal(await playback.wait(), 'completed');
    assert.equal(playback.state, 'completed');
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('decodes concurrent Readable sources in independent playback sessions', async () => {
  const remedia = await Remedia.create();
  const playbacks = [remedia.createPlaybackSession(), remedia.createPlaybackSession()];
  const sources = [0, 1].map(() =>
    Readable.from([makeWave(0.2)], { objectMode: false, highWaterMark: 1024 }),
  );

  try {
    const infos = await Promise.all(
      playbacks.map((playback, index) => playback.play({ stream: sources[index]! })),
    );
    assert.ok(infos.every((info) => info.channels === 1));
    assert.deepEqual(await Promise.all(playbacks.map((playback) => playback.wait())), [
      'completed',
      'completed',
    ]);
  } finally {
    await Promise.all(playbacks.map((playback) => playback.dispose()));
    await remedia.close();
  }
});

test('backpressures a slow Readable without blocking the Node event loop', async () => {
  const encoded = await readFile(fixture.path);
  const source = Readable.from(
    (async function* () {
      for (let offset = 0; offset < encoded.length; offset += 2048) {
        await new Promise((resolve) => setTimeout(resolve, 2));
        yield encoded.subarray(offset, Math.min(offset + 2048, encoded.length));
      }
    })(),
    { objectMode: false, highWaterMark: 2048 },
  );
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  let timerTicks = 0;
  const timer = setInterval(() => timerTicks++, 1);

  try {
    await playback.play({ stream: source });
    assert.equal(await playback.wait(), 'completed');
    assert.ok(timerTicks > 2);
  } finally {
    clearInterval(timer);
    await playback.dispose();
    await remedia.close();
  }
});

test('rejects object-mode Readable chunks with a useful source error', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  const source = Readable.from([{ not: 'bytes' }], { objectMode: true });

  try {
    await assert.rejects(playback.play({ stream: source }), /Buffer or Uint8Array/i);
    assert.equal(playback.state, 'failed');
    assert.match(playback.error ?? '', /Buffer or Uint8Array/i);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('propagates a Readable producer error while FFmpeg is probing', async () => {
  const source = Readable.from(
    (async function* () {
      yield Buffer.from('RIFF');
      throw new Error('synthetic Readable failure');
    })(),
    { objectMode: false, highWaterMark: 4 },
  );
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    await assert.rejects(playback.play({ stream: source }), /synthetic Readable failure/i);
    assert.equal(playback.state, 'failed');
    assert.match(playback.error ?? '', /synthetic Readable failure/i);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('stop and close cancel blocked Readable probes without destroying caller streams', async () => {
  const makeStalledSource = () => {
    let resolveReadRequested!: () => void;
    const readRequested = new Promise<void>((resolve) => {
      resolveReadRequested = resolve;
    });
    const stream = new Readable({
      objectMode: false,
      read() {
        // No bytes and no EOF: FFmpeg's initial input probe remains blocked
        // until native cancellation interrupts the byte-source read.
        resolveReadRequested();
      },
    });
    return { stream, readRequested };
  };

  const remedia = await Remedia.create();
  const stoppedPlayback = remedia.createPlaybackSession();
  const stoppedSource = makeStalledSource();
  const closedPlayback = remedia.createPlaybackSession();
  const closedSource = makeStalledSource();

  try {
    const stoppingPlay = stoppedPlayback.play({ stream: stoppedSource.stream }).then(
      () => 'resolved' as const,
      () => 'rejected' as const,
    );
    await Promise.all([
      stoppedSource.readRequested,
      waitFor(() => (stoppedPlayback.state === 'starting' ? true : undefined)),
    ]);
    await stoppedPlayback.stop();
    assert.equal(await stoppingPlay, 'rejected');
    assert.equal(stoppedPlayback.state, 'stopped');
    assert.equal(stoppedSource.stream.destroyed, false);

    const closingPlay = closedPlayback.play({ stream: closedSource.stream }).then(
      () => 'resolved' as const,
      () => 'rejected' as const,
    );
    await Promise.all([
      closedSource.readRequested,
      waitFor(() => (closedPlayback.state === 'starting' ? true : undefined)),
    ]);
    await remedia.close();
    assert.equal(await closingPlay, 'rejected');
    assert.equal(closedPlayback.state, 'stopped');
    assert.equal(closedSource.stream.destroyed, false);
  } finally {
    stoppedSource.stream.destroy();
    closedSource.stream.destroy();
    await Promise.all([stoppedPlayback.dispose(), closedPlayback.dispose()]);
    await remedia.close();
  }
});
