import assert from 'node:assert/strict';
import { afterAll, beforeAll, test } from 'vitest';

import { Remedia, type DiagnosticEvent } from '../lib/index.js';
import { createHeldAudioServer, createWaveFixture, makeWave } from './helpers.js';

let fixture: Awaited<ReturnType<typeof createWaveFixture>>;

beforeAll(async () => {
  fixture = await createWaveFixture(1);
});

afterAll(async () => {
  await fixture?.cleanup();
});

test('configures raw filtergraphs and independent master gain before playback', async () => {
  const remedia = await Remedia.create();
  const first = remedia.createPlaybackSession();
  const second = remedia.createPlaybackSession();

  try {
    await first.filters.set('bass=g=8:f=110,volume=0.8');
    await first.setVolume(0.75);
    await second.setVolume(0.5);
    assert.equal(first.filters.graph, 'bass=g=8:f=110,volume=0.8');
    assert.equal(first.volume, 0.75);
    assert.equal(second.filters.graph, null);
    assert.equal(second.volume, 0.5);

    await first.play({ buffer: fixture.wave });
    assert.equal(await first.wait(), 'completed');
    assert.ok(first.position.submittedMs > 0);
    assert.ok(first.position.mediaMs !== null && first.position.mediaMs > 0);

    await first.filters.clear();
    assert.equal(first.filters.graph, null);
    assert.equal(first.volume, 0.75);
    await assert.rejects(first.setVolume(Number.NaN), /finite linear gain/i);
    await assert.rejects(first.setVolume(2.01), /finite linear gain/i);
  } finally {
    await Promise.all([first.dispose(), second.dispose()]);
    await remedia.close();
  }
});

test('replaces a live graph safely and preserves the prior graph on validation failure', async () => {
  // Keep the HTTP response open after a small prefix so the native decoder is
  // waiting for more bytes from the same source while graph commands cross the
  // worker boundary. Each command releases more bytes only after native code
  // confirms the replacement is pending.
  const held = await createHeldAudioServer(makeWave(10), 512 * 1024);
  const remedia = await createRemediaWithPlaybackInfoDiagnostics();
  const playback = remedia.createPlaybackSession();

  try {
    const opening = playback.play({ url: held.url });
    await held.ready;
    await opening;
    assert.equal(playback.state, 'playing');
    await applyLiveFilter(remedia, playback, held, 'tremolo=f=5:d=0.6');
    await playback.setVolume(1.5);
    await assert.rejects(playback.filters.set('missing_filter=1'));
    assert.equal(playback.filters.graph, 'tremolo=f=5:d=0.6');
    assert.equal(playback.state, 'playing');
    assert.equal(playback.volume, 1.5);
    await applyLiveFilter(remedia, playback, held, 'vibrato=f=5:d=0.5');
    await applyLiveFilter(remedia, playback, held, null);
    assert.equal(playback.filters.graph, null);
    assert.equal(playback.volume, 1.5);
    held.finish();
    assert.equal(await playback.wait(), 'completed');
    assert.equal(held.requests, 1, 'filter changes must not reopen the HTTP input');
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});

async function createRemediaWithPlaybackInfoDiagnostics(): Promise<Remedia> {
  const previousLog = process.env.REMEDIA_LOG;
  const previousDebug = process.env.REMEDIA_DEBUG;
  process.env.REMEDIA_LOG = 'remedia_core::playback=info';
  try {
    return await Remedia.create();
  } finally {
    if (previousLog === undefined) delete process.env.REMEDIA_LOG;
    else process.env.REMEDIA_LOG = previousLog;
    if (previousDebug === undefined) delete process.env.REMEDIA_DEBUG;
    else process.env.REMEDIA_DEBUG = previousDebug;
  }
}

async function applyLiveFilter(
  remedia: Remedia,
  playback: ReturnType<Remedia['createPlaybackSession']>,
  held: Awaited<ReturnType<typeof createHeldAudioServer>>,
  expression: string | null,
): Promise<void> {
  const requested = waitForFilterRequest(remedia);
  const update = expression === null ? playback.filters.clear() : playback.filters.set(expression);
  await requested;
  assert.ok(held.sendNext() > 0, 'the held source has more bytes for the worker');
  await update;
  assert.equal(playback.state, 'playing');
}

function waitForFilterRequest(remedia: Remedia): Promise<void> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      remedia.off('debug', listener);
      reject(new Error('timed out waiting for the native filter transition request'));
    }, 2_000);
    const listener = (event: DiagnosticEvent) => {
      if (event.event !== 'core.playback.filter_graph_replacement_requested') return;
      clearTimeout(timeout);
      remedia.off('debug', listener);
      resolve();
    };
    remedia.on('debug', listener);
  });
}

test('Discord sessions retain independent graph and volume configuration before connect', async () => {
  const remedia = await Remedia.create({ send: () => {} });
  const first = remedia.createDiscordSession({
    guildId: '100000000000000001',
    channelId: '100000000000000002',
    userId: '100000000000000003',
  });
  const second = remedia.createDiscordSession({
    guildId: '200000000000000001',
    channelId: '200000000000000002',
    userId: '200000000000000003',
  });

  try {
    await first.filters.set('tremolo=f=5:d=0.6');
    await first.setVolume(0.25);
    await second.setVolume(1.75);
    assert.equal(first.filters.graph, 'tremolo=f=5:d=0.6');
    assert.equal(first.volume, 0.25);
    assert.equal(second.filters.graph, null);
    assert.equal(second.volume, 1.75);
  } finally {
    await Promise.all([first.dispose(), second.dispose()]);
    await remedia.close();
  }
});
