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
    position: {
      mediaMs,
      playedMs,
      submittedMs,
      mediaPositionAccuracy: mediaMs === null ? null : 'estimated',
    },
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

  it('reconciles output position without capping it to source duration', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.reconcile(snapshot(2, 'playing', 800, 1200, 1400, 1800));
    expect(clock.getPlaybackTime()).toBe(800);
    now = 600;
    expect(clock.getPlaybackTime()).toBe(1400);
    now = 2000;
    // Playback can outlast the source duration when tempo-changing filters slow it down.
    expect(clock.getPlaybackTime()).toBe(2800);
    clock.reconcile(snapshot(2, 'playing', null, 1100, 1300));
    expect(clock.getPlaybackTime()).toBe(1300);
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

  it('estimates source progress at the active tempo without rewriting prior time', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.reconcile({
      ...snapshot(1, 'playing', null, 5000, 4000),
      filterGraph: 'aresample=48000,asetrate=48000*1.25',
    });
    expect(clock.getPlaybackRate()).toBe(1.25);
    now = 2000;
    expect(clock.getMediaTime()).toBe(7500);
    clock.setFilterGraph('aresample=48000,asetrate=48000*0.8');
    expect(clock.getMediaTime()).toBe(7500);
    now = 3000;
    expect(clock.getMediaTime()).toBe(8300);
    clock.transition('paused');
    now = 5000;
    expect(clock.getMediaTime()).toBe(8300);
    clock.seek(12000, 2);
    expect(clock.getMediaTime()).toBe(12000);
    clock.transition('playing');
    now = 6000;
    expect(clock.getMediaTime()).toBe(12800);
    clock.suspend();
    now = 10000;
    expect(clock.getMediaTime()).toBe(12800);
  });

  it('accepts source positions when output samples are not reported', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.reconcile({ ...snapshot(4, 'playing', null, 1000), filterGraph: 'atempo=1.5' });
    now = 1000;
    expect(clock.getMediaTime()).toBe(2500);
    clock.reconcile({ ...snapshot(4, 'playing', null, null), filterGraph: 'atempo=1.5' });
    expect(clock.getMediaTime()).toBe(2500);
  });

  it('starts estimating after a timestamp-less initial snapshot', () => {
    let now = 0;
    const clock = new PlaybackClock(() => now);
    clock.reconcile({ ...snapshot(5, 'starting', null), filterGraph: 'atempo=1.25' });
    expect(clock.getMediaTime()).toBeNull();
    clock.start(5);
    now = 2000;
    expect(clock.getMediaTime()).toBe(2500);
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
