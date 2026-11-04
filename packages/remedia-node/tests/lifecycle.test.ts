import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import { afterAll, beforeAll, test } from 'vitest';

import { Remedia } from '../lib/index.js';
import {
  createHeldAudioServer,
  createWaveFixture,
  makeWave,
  pauseHeldPlayback,
} from './helpers.js';

let fixture: Awaited<ReturnType<typeof createWaveFixture>>;

beforeAll(async () => {
  fixture = await createWaveFixture();
});

afterAll(async () => {
  await fixture?.cleanup();
});

test('propagates invalid media failures into playback state', async () => {
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    await assert.rejects(playback.play({ file: `${fixture.path}.missing` }));
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
    await assert.rejects(failed.play({ file: `${fixture.path}.absent` }));
    await working.play({ file: fixture.path });
    assert.equal(await working.wait(), 'completed');
    assert.equal(failed.state, 'failed');
    assert.equal(working.state, 'completed');
  } finally {
    await Promise.all([failed.dispose(), working.dispose()]);
    await remedia.close();
  }
});

test('validates Discord snowflakes before creating a voice session', async () => {
  const remedia = await Remedia.create({ send: () => {} });

  try {
    assert.throws(
      () =>
        remedia.createDiscordSession({
          guildId: 'not-a-snowflake',
          channelId: '1',
          userId: '2',
        }),
      /guildId.*snowflake/i,
    );
  } finally {
    await remedia.close();
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
  if (!address || typeof address === 'string')
    throw new Error('test server did not bind a TCP port');

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

test('pause and resume gate playback while an HTTP source is held open', async () => {
  const held = await createHeldAudioServer(makeWave(30));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const starting = playback.play({ url: held.url });
    await held.ready;
    await starting;

    await pauseHeldPlayback(playback, held);
    assert.equal(playback.state, 'paused');

    // Resume while the response is still held open, so the unpaced decoder
    // cannot reach natural EOF before we observe the resumed state.
    await playback.resume();
    assert.equal(playback.state, 'playing');
    held.finish();
    assert.equal(await playback.wait(), 'completed');
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});

test('stop cancels a blocked HTTP read and releases playback resources', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const opening = playback.play({ url: held.url }).then(
      (info) => ({ info }),
      (error: unknown) => ({ error }),
    );
    await held.ready;
    assert.ok('info' in (await opening));
    assert.equal(playback.state, 'playing');
    const result = await Promise.race([
      playback.stop().then(() => 'stopped' as const),
      new Promise<'timeout'>((resolve) => setTimeout(() => resolve('timeout'), 2_000)),
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
    (error: unknown) => ({ error }),
  );

  try {
    await playback.dispose();
    const result = await opening;
    assert.ok('info' in result || 'error' in result);
    await assert.rejects(playback.play({ file: fixture.path }), /disposed/i);
  } finally {
    await remedia.close();
    await held.close();
  }
});

test('closing Remedia stops playback and settles waiters', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    await playback.play({ url: held.url });
    await held.ready;
    assert.equal(playback.state, 'playing');
    const waiting = playback.wait();
    const closing = await Promise.race([
      remedia.close().then(() => 'closed' as const),
      new Promise<'timeout'>((resolve) => setTimeout(() => resolve('timeout'), 2_000)),
    ]);
    assert.equal(closing, 'closed');
    assert.equal(await waiting, 'stopped');
    await held.waitForClientClose();
  } finally {
    await remedia.close();
    await held.close();
  }
});

test('completion waiters remain bound to the source active when wait was called', async () => {
  const held = await createHeldAudioServer(makeWave(10));
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const opening = playback.play({ url: held.url });
    await held.ready;
    await opening;
    const oldWait = playback.wait();
    await playback.stop();
    await playback.play({ buffer: fixture.wave });
    assert.equal(await oldWait, 'stopped');
    // The tiny generated fixture can reach EOF before this JS continuation is
    // scheduled on a loaded runner. Either nonterminal playback or its natural
    // completion proves the old waiter did not overwrite the new generation.
    assert.ok(playback.state === 'playing' || playback.state === 'completed');
    assert.equal(await playback.wait(), 'completed');
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});
