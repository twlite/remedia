import assert from 'node:assert/strict';
import { afterAll, beforeAll, test } from 'vitest';

import { Remedia } from '../lib/index.js';
import { createHeldAudioServer, createWaveFixture } from './helpers.js';

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
  const held = await createHeldAudioServer(fixture.wave);
  const remedia = await Remedia.create();
  const playback = remedia.createPlaybackSession();

  try {
    const opening = playback.play({ url: held.url });
    await held.ready;
    await opening;
    await playback.filters.set('tremolo=f=5:d=0.6');
    await playback.setVolume(1.5);
    await assert.rejects(playback.filters.set('missing_filter=1'));
    assert.equal(playback.filters.graph, 'tremolo=f=5:d=0.6');
    assert.equal(playback.volume, 1.5);
    await playback.filters.set('vibrato=f=5:d=0.5');
    await playback.filters.clear();
    assert.equal(playback.filters.graph, null);
    assert.equal(playback.volume, 1.5);
    held.finish();
    assert.equal(await playback.wait(), 'completed');
  } finally {
    await playback.dispose();
    await remedia.close();
    await held.close();
  }
});

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
