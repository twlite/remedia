import { describe, expect, it } from 'vitest';
import { PlaybackClock } from '../lib/playback-clock.js';
import type { PlaybackSnapshot, PlaybackState } from '../lib/types.js';

function snapshot(
  generation: number,
  state: PlaybackState,
  playedMs: number | null,
  mediaMs: number | null = null,
  submittedMs = 0,
  durationMs: number | null = null,
): PlaybackSnapshot {
  return {
    generation,
    state,
    processingMode: null,
    position: { mediaMs, playedMs, submittedMs, mediaPositionAccuracy: mediaMs === null ? null : 'estimated' },
    durationMs,
    source: null,
    filterGraph: null,
    volume: 1,
    lastError: null,
  };
}

describe('remote playback clock', () => {
  it('advances locally and freezes on pause, stop and disconnection', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    expect(clock.getPlaybackTime()).toBe(0);
    clock.start(1);
    now = 1_250;
    expect(clock.getPlaybackTime()).toBe(1_250);
    clock.transition('paused');
    now = 3_000;
    expect(clock.getPlaybackTime()).toBe(1_250);
    clock.transition('playing');
    now = 3_600;
    expect(clock.getPlaybackTime()).toBe(1_850);
    clock.suspend();
    now = 10_000;
    expect(clock.getPlaybackTime()).toBe(1_850);
    clock.reconcile(snapshot(1, 'playing', 2_100));
    now = 10_400;
    expect(clock.getPlaybackTime()).toBe(2_500);
    clock.transition('stopped');
    now = 15_000;
    expect(clock.getPlaybackTime()).toBe(2_500);
  });

  it('reconciles with output, media, then submitted position and clamps finite sources', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.reconcile(snapshot(2, 'playing', 800, 1200, 1400, 1800));
    expect(clock.getPlaybackTime()).toBe(800);
    now = 600;
    expect(clock.getPlaybackTime()).toBe(1400);
    now = 2000;
    expect(clock.getPlaybackTime()).toBe(1800);
    clock.reconcile(snapshot(2, 'playing', null, 1100, 1300));
    expect(clock.getPlaybackTime()).toBe(1100);
    clock.reconcile(snapshot(2, 'playing', null, null, 1500));
    expect(clock.getPlaybackTime()).toBe(1500);
  });

  it('handles seeking, generations and stale snapshot rejection', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.start(1);
    now = 400;
    clock.seek(30_000, 2);
    expect(clock.getPlaybackTime()).toBe(30_000);
    now = 900;
    expect(clock.getPlaybackTime()).toBe(30_500);
    clock.reconcile(snapshot(1, 'paused', 123));
    expect(clock.getPlaybackTime()).toBe(30_500);
    clock.start(3);
    expect(clock.getPlaybackTime()).toBe(0);
    now = 1200;
    expect(clock.getPlaybackTime()).toBe(300);
    clock.reconcile(snapshot(2, 'playing', 1300, null), true);
    expect(clock.getPlaybackTime()).toBe(1300);
  });

  it('does not advance while the authoritative server state is paused', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.reconcile(snapshot(1, 'paused', 500));
    now = 10_000;
    expect(clock.getPlaybackTime()).toBe(500);
    clock.transition('playing');
    now = 10_020;
    expect(clock.getPlaybackTime()).toBe(520);
  });
});
