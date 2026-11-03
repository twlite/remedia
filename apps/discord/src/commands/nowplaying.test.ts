import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { AudioStreamInfo } from '@remedia/client';
import type { PlaybackSnapshot } from '../voice/playback-manager.js';
import { createNowPlayingEmbed } from './nowplaying.js';
import { createProgressBar, formatProgressLine, formatTimecode } from '../voice/progress.js';

test('progress bar positions its indicator proportionally and clamps invalid positions', () => {
  assert.equal(createProgressBar(50, 100, 9), '▬▬▬▬●────');
  assert.equal(createProgressBar(0, 100, 4), '●───');
  assert.equal(createProgressBar(100, 100, 4), '▬▬▬●');
  assert.equal(createProgressBar(150, 100, 4), '▬▬▬●');
  assert.equal(createProgressBar(Number.NaN, 100, 4), '●───');
  assert.equal(createProgressBar(10, 0), null);
  assert.equal(createProgressBar(10, Number.POSITIVE_INFINITY), null);
});

test('timecodes use mm:ss and hh:mm:ss and progress never exceeds duration', () => {
  assert.equal(formatTimecode(84_000), '01:24');
  assert.equal(formatTimecode(3_723_000), '01:02:03');
  assert.equal(formatTimecode(Number.NaN), 'unknown');
  assert.equal(formatProgressLine(999_000, 200_000), `03:20 ┃ ${'▬'.repeat(17)}● ┃ 03:20`);
  assert.equal(formatProgressLine(84_000, null, { estimated: true }), '~01:24 ┃ Duration unknown');
});

test('now-playing embed shows source progress, requester, queue, filters, and volume', () => {
  const embed = createNowPlayingEmbed(
    makeSnapshot({
      queue: [
        { url: 'https://media.example/next.mp3', requestedBy: 'other' },
        { url: 'https://media.example/last.mp3', requestedBy: 'other' },
      ],
    }),
  );
  const data = embed.toJSON();

  assert.equal(data.title, 'Example Song');
  assert.match(data.description ?? '', /~00:48 ┃ [▬─●]+ ┃ 03:20/);
  assert.deepEqual(
    data.fields?.map(({ name, value }) => [name, value]),
    [
      ['Requested by', '<@user>'],
      ['State', 'Playing'],
      ['Up next', '2 tracks'],
      ['Volume', '75%'],
      ['Filters', 'nightcore'],
      ['Loop', 'Track'],
    ],
  );
  assert.equal(data.color, 0x57f287);
  assert.equal(data.footer?.text, 'Source position is estimated');
});

test('now-playing shows every active filter in selection order', () => {
  const embed = createNowPlayingEmbed(
    makeSnapshot({ filterPresets: ['bassboost', 'tremolo', 'vibrato'] }),
  );

  assert.equal(
    embed.data.fields?.find(({ name }) => name === 'Filters')?.value,
    'bassboost, tremolo, vibrato',
  );
});

test('unknown and zero durations do not show a fabricated progress bar', () => {
  for (const durationMs of [null, 0, Number.NaN]) {
    const snapshot = makeSnapshot({ durationMs });
    if (snapshot.current?.info) snapshot.current.info.durationMs = null;
    const embed = createNowPlayingEmbed(snapshot);
    assert.equal(embed.data.description, '`~00:48 ┃ Duration unknown`');
  }
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
    filterPresets: ['nightcore'],
    volumePercent: 75,
    loopMode: 'track',
    ...overrides,
  };
}
