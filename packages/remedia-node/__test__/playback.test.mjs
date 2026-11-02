import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test, { after, before } from 'node:test';

import { Remedia } from '../dist/index.js';

let directory;
let wavePath;

before(async () => {
  directory = await mkdtemp(join(tmpdir(), 'remedia-node-'));
  wavePath = join(directory, 'tone.wav');
  await writeFile(wavePath, makeWave(0.25));
});

after(async () => {
  if (directory) await rm(directory, { recursive: true, force: true });
});

test('plays a local file, reports metadata and completes incrementally', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const stream = await playback.play({ file: wavePath });
    assert.equal(stream.sampleRate, 48_000);
    assert.ok(stream.channels === 1 || stream.channels === 2);
    assert.equal(await playback.wait(), 'completed');
    assert.equal(playback.state, 'completed');
    assert.ok(playback.durationMs !== null && playback.durationMs > 0);
    assert.ok(playback.position.submittedMs > 0);
    assert.equal(playback.error, null);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('propagates invalid media errors into the playback state', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    await assert.rejects(playback.play({ file: join(directory, 'missing.wav') }));
    assert.equal(playback.state, 'failed');
    assert.match(playback.error ?? '', /media source was not found/i);
    await assert.rejects(playback.pause());
    await assert.rejects(playback.seek(100));
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('keeps independent playback sessions isolated after one fails', async () => {
  const remedia = await Remedia.create();
  const failed = remedia.createPlaybackSession();
  const working = remedia.createPlaybackSession();

  try {
    await assert.rejects(failed.play({ file: join(directory, 'absent.wav') }));
    await working.play({ file: wavePath });
    assert.equal(await working.wait(), 'completed');
    assert.equal(failed.state, 'failed');
    assert.equal(working.state, 'completed');
  } finally {
    await Promise.all([failed.dispose(), working.dispose()]);
    await remedia.close();
  }
});

test('validates Discord snowflakes before attempting a network connection', async () => {
  const remedia = await Remedia.create();

  try {
    await assert.rejects(
      remedia.connectVoice({
        guildId: 'not-a-snowflake',
        channelId: '1',
        userId: '2',
        sessionId: 'voice-session',
        token: 'not-a-real-token',
        endpoint: '127.0.0.1:9',
      }),
      /guildId.*snowflake/i,
    );
  } finally {
    await remedia.close();
  }
});

test('delivers structured diagnostics and honors listener removal', async () => {
  const priorFilter = process.env.REMEDIA_LOG;
  process.env.REMEDIA_LOG = 'remedia_core=info';
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  const received = [];
  const listener = (event) => received.push(event);
  remedia.on('debug', listener);

  try {
    await playback.play({ file: wavePath });
    await playback.wait();
    const event = await waitFor(() =>
      received.find((item) => item.event === 'core.playback.starting'),
    );
    assert.equal(event.severity, 'info');
    assert.equal(event.target, 'remedia_core::playback');
    assert.equal(typeof event.timestamp, 'number');
    assert.ok(event.playbackId);
    assert.equal(event.fields.source_kind.kind, 'string');
    assert.equal(event.fields.source_kind.value, '[REDACTED]');
    assert.equal(typeof remedia.droppedDebugEvents, 'number');

    remedia.off('debug', listener);
    const countAfterRemoval = received.length;
    const second = remedia.createPlaybackSession();
    try {
      await second.play({ file: wavePath });
      await second.wait();
      await new Promise((resolve) => setTimeout(resolve, 40));
      assert.equal(received.length, countAfterRemoval);
    } finally {
      await second.dispose();
    }
  } finally {
    await playback.dispose();
    await remedia.close();
    if (priorFilter === undefined) delete process.env.REMEDIA_LOG;
    else process.env.REMEDIA_LOG = priorFilter;
  }
});

test('native opening work does not stall the Node event loop', async () => {
  const responseWave = makeWave(0.15);
  const server = createServer((_request, response) => {
    setTimeout(() => {
      response.writeHead(200, {
        'content-length': responseWave.length,
        'content-type': 'audio/wav',
      });
      response.end(responseWave);
    }, 80);
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address === 'object');

  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  let timerTicks = 0;
  const timer = setInterval(() => timerTicks++, 5);

  try {
    await playback.play({ url: `http://127.0.0.1:${address.port}/audio.wav` });
    await playback.wait();
    assert.ok(timerTicks >= 3, `expected event-loop ticks while HTTP opened, got ${timerTicks}`);
  } finally {
    clearInterval(timer);
    await playback.dispose();
    await remedia.close();
    server.close();
    await once(server, 'close');
  }
});

test('pause and resume gate decoding while an HTTP source is held open', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const starting = playback.play({ url: held.url });
    await held.ready;
    await starting;

    const pausing = playback.pause();
    await waitFor(() => playback.state === 'pausing' || playback.state === 'paused');
    held.finish();
    await pausing;
    assert.equal(playback.state, 'paused');
    assert.ok(playback.position.submittedMs >= 0);

    await playback.resume();
    assert.equal(playback.state, 'playing');
    assert.equal(await playback.wait(), 'completed');
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});

test('stop cancels an in-flight HTTP read and releases the playback worker', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const opening = playback.play({ url: held.url }).then(
      (info) => ({ info }),
      (error) => ({ error }),
    );
    await held.ready;
    const startup = await opening;
    assert.ok(startup.info);
    assert.equal(playback.state, 'playing');
    const result = await Promise.race([
      playback.stop().then(() => 'stopped'),
      new Promise((resolve) => setTimeout(() => resolve('timeout'), 2_000)),
    ]);
    assert.equal(result, 'stopped');
    assert.equal(playback.state, 'stopped');
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});

test('dispose cancels an in-flight play and rejects later operations', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  const opening = playback.play({ url: held.url }).then(
    (info) => ({ info }),
    (error) => ({ error }),
  );

  try {
    await playback.dispose();
    const result = await opening;
    assert.ok(result.info || result.error);
    await assert.rejects(playback.play({ file: wavePath }), /disposed/i);
  } finally {
    await remedia.close();
    await held.close();
  }
});

test('close stops active playback and settles an outstanding wait operation', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    await playback.play({ url: held.url });
    await held.ready;
    assert.equal(playback.state, 'playing');
    const waiting = playback.wait();
    const closing = await Promise.race([
      remedia.close().then(() => 'closed'),
      new Promise((resolve) => setTimeout(() => resolve('timeout'), 2_000)),
    ]);
    assert.equal(closing, 'closed');
    assert.equal(await waiting, 'stopped');
    await held.waitForClientClose();
  } finally {
    await remedia.close();
    await held.close();
  }
});

test('component filters suppress events from unrelated Rust crates', async () => {
  const priorFilter = process.env.REMEDIA_LOG;
  process.env.REMEDIA_LOG = 'remedia_discord=info';
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();
  const events = [];
  remedia.on('debug', (event) => events.push(event));

  try {
    await playback.play({ file: wavePath });
    await playback.wait();
    await new Promise((resolve) => setTimeout(resolve, 60));
    assert.deepEqual(events, []);
  } finally {
    await playback.dispose();
    await remedia.close();
    if (priorFilter === undefined) delete process.env.REMEDIA_LOG;
    else process.env.REMEDIA_LOG = priorFilter;
  }
});

test('bounded diagnostic forwarding counts events dropped during JavaScript backpressure', async () => {
  const priorFilter = process.env.REMEDIA_LOG;
  process.env.REMEDIA_LOG = 'remedia_core=info';
  const remedia = await Remedia.create();
  const sessions = Array.from({ length: 64 }, () => remedia.createPlaybackSession());
  remedia.on('debug', () => {});

  try {
    const starting = sessions.map((session) => session.play({ file: wavePath }));
    // Hold Node's callback queue while the independent Rust workers produce lifecycle events.
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 300);
    const results = await Promise.allSettled(starting);
    assert.ok(results.some((result) => result.status === 'fulfilled'));
    await Promise.all(sessions.map((session) => session.wait()));
    assert.ok(remedia.droppedDebugEvents > 0);
  } finally {
    await Promise.all(sessions.map((session) => session.dispose()));
    await remedia.close();
    if (priorFilter === undefined) delete process.env.REMEDIA_LOG;
    else process.env.REMEDIA_LOG = priorFilter;
  }
});

function makeWave(durationSeconds, sampleRate = 48_000, channels = 1) {
  const samplesPerChannel = Math.round(durationSeconds * sampleRate);
  const bytesPerSample = 2;
  const dataLength = samplesPerChannel * channels * bytesPerSample;
  const wave = Buffer.alloc(44 + dataLength);
  wave.write('RIFF', 0);
  wave.writeUInt32LE(36 + dataLength, 4);
  wave.write('WAVE', 8);
  wave.write('fmt ', 12);
  wave.writeUInt32LE(16, 16);
  wave.writeUInt16LE(1, 20);
  wave.writeUInt16LE(channels, 22);
  wave.writeUInt32LE(sampleRate, 24);
  wave.writeUInt32LE(sampleRate * channels * bytesPerSample, 28);
  wave.writeUInt16LE(channels * bytesPerSample, 32);
  wave.writeUInt16LE(bytesPerSample * 8, 34);
  wave.write('data', 36);
  wave.writeUInt32LE(dataLength, 40);

  for (let index = 0; index < samplesPerChannel * channels; index++) {
    const sample = Math.round(Math.sin((index / sampleRate) * Math.PI * 440 * 2) * 12_000);
    wave.writeInt16LE(sample, 44 + index * bytesPerSample);
  }
  return wave;
}

async function createHeldAudioServer(wave, prefixBytes = 512 * 1024) {
  let response;
  let readyResolve;
  let clientCloseResolve;
  const ready = new Promise((resolve) => (readyResolve = resolve));
  const clientClosed = new Promise((resolve) => (clientCloseResolve = resolve));
  const server = createServer((_request, outgoing) => {
    response = outgoing;
    outgoing.on('close', clientCloseResolve);
    outgoing.writeHead(200, {
      'content-length': wave.length,
      'content-type': 'audio/wav',
    });
    outgoing.write(wave.subarray(0, prefixBytes));
    readyResolve();
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address === 'object');

  return {
    url: `http://127.0.0.1:${address.port}/held.wav`,
    ready,
    finish() {
      response?.end(wave.subarray(prefixBytes));
    },
    async waitForClientClose() {
      await Promise.race([
        clientClosed,
        new Promise((_, reject) =>
          setTimeout(() => reject(new Error('HTTP input was not closed')), 2_000),
        ),
      ]);
    },
    async close() {
      response?.destroy();
      if (!server.listening) return;
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

async function waitFor(findValue, timeoutMs = 2_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = findValue();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('timed out waiting for native diagnostic event');
}
