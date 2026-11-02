import { NativeRemedia } from '../native.js';
import type { NativeMediaSource } from '../native.js';
import {
  createReadableInput,
  toNativeSource,
  type MediaSource,
  type ReadableInputHandle,
} from './input.js';
import {
  toStreamInfo,
  type AudioStreamInfo,
  type PlaybackPosition,
  type PlaybackState,
} from './types.js';

/** Controls one playback session. Local sessions decode into a no-op output. */
export class PlaybackSession {
  protected readonly native: NativeRemedia;
  protected readonly playbackId: string;
  readonly #onDispose: (() => void) | undefined;
  #input: ReadableInputHandle | undefined;
  #disposed = false;

  /** @internal */
  constructor(native: NativeRemedia, playbackId: string, onDispose?: () => void) {
    this.native = native;
    this.playbackId = playbackId;
    this.#onDispose = onDispose;
  }

  get state(): PlaybackState {
    return this.native.playbackState(this.playbackId) as PlaybackState;
  }

  get position(): PlaybackPosition {
    const position = this.native.playbackPosition(this.playbackId);
    return { submittedMs: position.submittedMs, playedMs: position.playedMs ?? null };
  }

  get durationMs(): number | null {
    return this.native.playbackDurationMs(this.playbackId) ?? null;
  }

  get error(): string | null {
    return this.native.playbackError(this.playbackId) ?? null;
  }

  async play(source: MediaSource): Promise<AudioStreamInfo> {
    this.#assertActive();
    if (this.#input && isPlaybackActive(this.state)) {
      throw new Error('playback is already active; stop it before starting another source');
    }
    await this.#releaseInput();

    let input: ReadableInputHandle | undefined;
    let nativeSource: NativeMediaSource;
    if ('stream' in source) {
      input = createReadableInput(this.native, source.stream);
      nativeSource = input.source;
      this.#input = input;
    } else {
      nativeSource = toNativeSource(source);
    }

    try {
      return toStreamInfo(await this.native.play(this.playbackId, nativeSource));
    } catch (error) {
      await this.#releaseInput();
      throw error;
    }
  }

  async pause(): Promise<void> {
    this.#assertActive();
    await this.native.pause(this.playbackId);
  }

  async resume(): Promise<void> {
    this.#assertActive();
    await this.native.resume(this.playbackId);
  }

  async seek(positionMs: number): Promise<void> {
    this.#assertActive();
    await this.native.seek(this.playbackId, positionMs);
  }

  async stop(): Promise<void> {
    this.#assertActive();
    try {
      await this.native.stop(this.playbackId);
    } finally {
      await this.#releaseInput();
    }
  }

  async wait(): Promise<PlaybackState> {
    this.#assertActive();
    try {
      return (await this.native.wait(this.playbackId)) as PlaybackState;
    } finally {
      await this.#releaseInput();
    }
  }

  /** Stop and release this standalone playback session. */
  async dispose(): Promise<void> {
    if (this.#disposed) return;
    try {
      await this.native.disposePlaybackSession(this.playbackId);
    } finally {
      await this.#releaseInput();
      this.markDisposed();
    }
  }

  protected get isDisposed(): boolean {
    return this.#disposed;
  }

  protected markDisposed(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#onDispose?.();
  }

  /** @internal */
  async releaseInput(): Promise<void> {
    await this.#releaseInput();
  }

  async #releaseInput(): Promise<void> {
    const input = this.#input;
    this.#input = undefined;
    if (input) await input.cancel();
  }

  #assertActive(): void {
    if (this.#disposed) throw new Error('Playback session has been disposed');
  }
}

function isPlaybackActive(state: PlaybackState): boolean {
  return (
    state === 'starting' ||
    state === 'playing' ||
    state === 'pausing' ||
    state === 'resuming' ||
    state === 'paused' ||
    state === 'stopping'
  );
}
