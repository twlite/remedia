import assert from 'node:assert/strict';
import { afterEach, test, vi } from 'vitest';

import { Remedia } from '../lib/index.js';
import type { DiagnosticEvent } from '../lib/index.js';
import { makeWave } from './helpers.js';

afterEach(() => {
  vi.unstubAllEnvs();
});

test('forwards structured events with correlation fields and supports listener removal', async () => {
  vi.stubEnv('REMEDIA_LOG', 'remedia_core=info');
  const remedia = await Remedia.create();
  const events: DiagnosticEvent[] = [];
  const listener = (event: DiagnosticEvent) => events.push(event);
  remedia.on('debug', listener);
  const playback = remedia.createPlaybackSession();

  try {
    await playback.play({ buffer: makeWave(0.1) });
    await playback.wait();

    const deadline = Date.now() + 2_000;
    while (
      !events.some((event) => event.event === 'core.playback.started') &&
      Date.now() < deadline
    ) {
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    const started = events.find((event) => event.event === 'core.playback.started');
    assert.ok(started, 'expected a playback-start diagnostic event');
    assert.equal(started.severity, 'info');
    assert.equal(started.target, 'remedia_core::playback');
    assert.ok(started.playbackId);
    assert.equal(started.fields.codec?.kind, 'debug');

    assert.equal(remedia.off('debug', listener), remedia);
    assert.equal(remedia.removeAllListeners('debug'), remedia);
    assert.ok(Number.isInteger(remedia.droppedDebugEvents));
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});

test('REMEDIA_LOG filters events before they reach JavaScript listeners', async () => {
  vi.stubEnv('REMEDIA_LOG', 'remedia_core=error');
  const remedia = await Remedia.create();
  const events: string[] = [];
  remedia.on('debug', (event) => events.push(event.event));
  const playback = remedia.createPlaybackSession();

  try {
    await playback.play({ buffer: makeWave(0.05) });
    await playback.wait();
    await new Promise((resolve) => setTimeout(resolve, 75));
    assert.deepEqual(events, []);
  } finally {
    await playback.dispose();
    await remedia.close();
  }
});
