import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { PassThrough, Readable } from 'node:stream';
import { test } from 'vitest';
import { Remedia } from '../lib/index.js';
import { createWaveFixture, makeWave, pauseHeldPlayback } from './helpers.js';

test('probes file, local HTTP, Buffer, Uint8Array and disposable one-shot streams without playback events', async () => {
  const fixture = await createWaveFixture();
  const bytes = await readFile(fixture.path);
  const engine = await Remedia.create();
  const server = createServer((_request, response) => response.end(bytes));
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  let events = 0;
  engine.on('playbackStarted', () => events++);
  try {
    for (const source of [
      { file: fixture.path },
      { url: `http://127.0.0.1:${address.port}/audio` },
      { buffer: bytes },
      { buffer: new Uint8Array(bytes) },
      { stream: Readable.from([bytes], { autoDestroy: false }) },
    ]) {
      const info = await engine.probe(source);
      assert.equal(info.sampleRate, 48000);
      assert.equal(info.channels, 1);
      assert.equal(info.metadata.title, undefined);
      assert.equal(info.metadata.artist, undefined);
    }
    const stream = Readable.from([bytes], { autoDestroy: false });
    await engine.probe({ stream });
    assert.equal(stream.destroyed, false);
    for (const event of ['readable', 'end', 'close', 'error'])
      assert.equal(stream.listenerCount(event), 0);
    const playback = engine.createPlaybackSession();
    const before = playback.state;
    await Promise.all(Array.from({ length: 4 }, () => engine.probe({ buffer: bytes })));
    assert.equal(playback.state, before);
    assert.equal(events, 0);
    await playback.play({ stream: Readable.from([bytes]) });
    assert.equal(await playback.wait(), 'completed');
    await playback.dispose();
  } finally {
    await engine.close();
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    await fixture.cleanup();
  }
});

test('stalled streams time out, abort and detach without destroying caller input', async () => {
  const engine = await Remedia.create();
  try {
    for (const abort of [false, true]) {
      const stream = new Readable({ read() {} });
      const controller = new AbortController();
      const operation = engine.probe(
        { stream },
        { timeoutMs: abort ? 2000 : 30, signal: controller.signal },
      );
      if (abort) {
        await new Promise<void>((resolve) => setTimeout(resolve, 30));
        controller.abort();
      }
      await assert.rejects(operation, abort ? /abort/i : /timed out/i);
      assert.equal(stream.destroyed, false);
      for (const event of ['readable', 'end', 'close', 'error'])
        assert.equal(stream.listenerCount(event), 0);
    }
    await assert.rejects(engine.probe({ buffer: Buffer.from('invalid media') }));
    await assert.rejects(
      engine.probe({
        buffer: Buffer.from(
          'YUV4MPEG2 W2 H2 F25:1 Ip A1:1 C420jpeg\nFRAME\n\x10\x10\x10\x10\x80\x80',
          'binary',
        ),
      }),
      /audio stream/,
    );
    const pending = engine.probe({ stream: new Readable({ read() {} }) });
    const rejected = assert.rejects(pending, /abort/i);
    await engine.close();
    await rejected;
  } finally {
    await engine.close();
  }
});

test('unknown non-seekable duration remains null and probes preserve active playback', async () => {
  const engine = await Remedia.create();
  const bytes = makeWave(0.1);
  try {
    const unknown = Buffer.from(bytes);
    unknown.writeUInt32LE(0xffffffff, 4);
    unknown.writeUInt32LE(0xffffffff, 40);
    const info = await engine.probe({ stream: Readable.from([unknown]) });
    assert.equal(info.durationMs, null);
    // Feed the active playback incrementally. A null output can decode a
    // fully pushed stream before pause() runs, leaving the worker blocked on
    // the next input read and unable to acknowledge the pause request.
    const encoded = makeWave(5);
    const stream = new PassThrough({ highWaterMark: 1024 });
    let offset = 44 + 48_000 * 2;
    stream.write(encoded.subarray(0, offset));
    const playback = engine.createPlaybackSession();
    try {
      await playback.play({ stream });
      await pauseHeldPlayback(playback, {
        sendNext(chunkBytes = 8192): number {
          const end = Math.min(offset + chunkBytes, encoded.length);
          const count = end - offset;
          if (count) stream.write(encoded.subarray(offset, end));
          offset = end;
          return count;
        },
      });
      const state = playback.state;
      const generation = playback.currentGeneration;
      await engine.probe({ buffer: bytes });
      assert.equal(playback.state, state);
      assert.equal(playback.currentGeneration, generation);
      await playback.stop();
    } finally {
      await playback.dispose();
      stream.destroy();
    }
  } finally {
    await engine.close();
  }
});

test('probe capacity and producer errors release inputs and allow subsequent probes', async () => {
  const engine = await Remedia.create();
  const controllers = Array.from({ length: 8 }, () => new AbortController());
  const streams = controllers.map(() => new Readable({ read() {} }));
  const pending = controllers.map((controller, index) =>
    engine
      .probe({ stream: streams[index]! }, { signal: controller.signal })
      .catch((error) => error),
  );
  try {
    await assert.rejects(engine.probe({ buffer: makeWave(0.1) }), /concurrency limit/);
    for (const controller of controllers) controller.abort();
    await Promise.all(pending);
    for (const stream of streams) {
      assert.equal(stream.destroyed, false);
      assert.equal(stream.listenerCount('error'), 0);
    }
    const failed = new Readable({
      read() {
        this.emit('error', new Error('probe producer failure'));
      },
    });
    await assert.rejects(engine.probe({ stream: failed }), /probe producer failure/);
    assert.equal(failed.listenerCount('error'), 0);
    assert.equal((await engine.probe({ buffer: makeWave(0.1) })).sampleRate, 48000);
  } finally {
    for (const controller of controllers) controller.abort();
    await Promise.all(pending);
    await engine.close();
  }
});
