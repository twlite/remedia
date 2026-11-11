import { performance } from 'node:perf_hooks';
import type { PlaybackSnapshot, PlaybackState } from './types.js';

/**
 * On-demand clock for an assigned remote playback generation.
 * Server positions are authoritative. Between observations, elapsed output time is
 * extrapolated using a monotonic clock; no polling or background timer is required.
 */
export class PlaybackClock {
  #generation = -1;
  #positionMs = 0;
  #sampledAtMs: number;
  readonly #now: () => number;

  constructor(now: () => number = () => performance.now()) {
    this.#now = now;
    this.#sampledAtMs = now();
  }
  #state: PlaybackState = 'idle';
  #suspended = false;

  get generation(): number {
    return this.#generation;
  }

  start(generation: number): void {
    if (generation < this.#generation) return;
    if (generation !== this.#generation) {
      // A new track has its own timeline. Do not carry elapsed time from the
      // previous generation, including any time spent paused or disconnected.
      this.#positionMs = 0;
      this.#generation = generation;
      this.#sampledAtMs = this.#now();
      this.#suspended = false;
      this.#state = 'playing';
      return;
    }
    this.transition('playing');
  }

  /** Freeze at the current estimate, or resume from it, on lifecycle changes. */
  transition(state: PlaybackState): void {
    const now = this.#now();
    this.#positionMs = this.#readAt(now);
    this.#sampledAtMs = now;
    this.#state = state;
  }

  /** Seek positions refer to the output timeline at the new source boundary. */
  seek(positionMs: number, generation?: number): void {
    if (generation !== undefined) {
      if (generation < this.#generation) return;
      this.#generation = generation;
    }
    if (!Number.isFinite(positionMs) || positionMs < 0) return;
    this.#positionMs = positionMs;
    this.#sampledAtMs = this.#now();
  }

  /** A lost transport must not extrapolate indefinitely without knowing whether audio continued. */
  suspend(): void {
    const now = this.#now();
    this.#positionMs = this.#readAt(now);
    this.#sampledAtMs = now;
    this.#suspended = true;
  }

  /** Replace local drift with an authoritative snapshot, including resume/recovery. */
  reconcile(snapshot: PlaybackSnapshot, force = false): void {
    if (snapshot.generation < this.#generation && !force) return;
    const nextGeneration = snapshot.generation !== this.#generation;
    const observed =
      snapshot.position.playedMs ??
      snapshot.position.mediaMs ??
      (snapshot.position.submittedMs > 0 ? snapshot.position.submittedMs : null);
    const now = this.#now();
    const previousEstimate = this.#readAt(now);
    this.#generation = snapshot.generation;
    this.#state = snapshot.state;
    this.#positionMs = observed ?? (nextGeneration ? 0 : previousEstimate);
    this.#sampledAtMs = now;
    this.#suspended = false;
  }

  /** Elapsed output time in ms; source duration is not a valid cap for tempo-changing filters. */
  getPlaybackTime(): number {
    return this.#readAt(this.#now());
  }

  #readAt(now: number): number {
    const active = this.#state === 'playing' && !this.#suspended;
    const elapsed = active ? Math.max(0, now - this.#sampledAtMs) : 0;
    const position = Math.max(0, this.#positionMs + elapsed);
    return position;
  }
}
