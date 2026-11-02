import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { AudioStreamInfo } from '@remedia/node';
import type { PlaybackSnapshot } from '../voice/playback-manager.js';
import { createNowPlayingEmbed, makeProgressBar } from './nowplaying.js';

test('now-playing progress bar places its marker proportionally and clamps the ends', () => {
  assert.equal(makeProgressBar(50, 100, 10), '▰▰▰▰▰●▱▱▱▱');
  assert.equal(makeProgressBar(0, 100, 4), '●▱▱▱');
  assert.equal(makeProgressBar(100, 100, 4), '▰▰▰●');
  assert.equal(makeProgressBar(150, 100, 4), '▰▰▰●');
});

test('now-playing embed shows the source position, progress bar, state, filter, and volume', () => {
  const embed = createNowPlayingEmbed(makeSnapshot());
  const data = embed.toJSON();

  assert.equal(data.title, 'Example Song');
  assert.match(data.description ?? '', /▰▰▰▰●▱{13}/);
  assert.match(data.description ?? '', /~0:48.*3:20/);
  assert.deepEqual(
    data.fields?.map(({ name, value }) => [name, value]),
    [
      ['Status', 'Playing'],
      ['Filter', 'nightcore'],
      ['Volume', '75%'],
    ],
  );
  assert.equal(data.color, 0x57f287);
  assert.equal(data.footer?.text, 'Source position is estimated');
});

test('unknown duration is shown without implying a measured percentage', () => {
  const embed = createNowPlayingEmbed(
    makeSnapshot({
      durationMs: null,
      current: {
        url: 'https://media.example/example.wav',
        requestedBy: 'user',
        info: undefined,
      },
    }),
  );
  assert.equal(embed.data.description, '**~0:48** / **unknown**');
});

function makeSnapshot(overrides: Partial<PlaybackSnapshot> = {}): PlaybackSnapshot {
  return {
    channelId: 'channel',
    connectionState: 'connected',
    state: 'playing',
    positionMs: 48_000,
    positionEstimated: true,
    durationMs: 200_000,
    current: {
      url: 'https://media.example/example.wav',
      requestedBy: 'user',
      info: {
        streamIndex: 0,
        codec: 'wav',
        sampleRate: 48_000,
        channels: 2,
        channelLayout: 'stereo',
        durationMs: 200_000,
        bitRate: null,
        metadata: { title: 'Example Song' },
      } satisfies AudioStreamInfo,
    },
    queue: [],
    lastError: null,
    filterPreset: 'nightcore',
    volumePercent: 75,
    ...overrides,
  };
}
