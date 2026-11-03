import { RemediaEventEmitter } from './events.js';
import { NativeRemedia } from '../native.js';
import type { NativeMediaSource } from '../native.js';
import {
  createReadableInput,
  toNativeSource,
  type MediaSource,
  type ReadableInputHandle,
} from './input.js';
import {
  lifecycleContext,
  PlaybackHooks,
  PlaybackSessionEvents as PlaybackEvent,
  type PlayOptions,
  type PlaybackSessionEventMap,
} from './lifecycle.js';
import {
  toStreamInfo,
  type AudioStreamInfo,
  type PlaybackPosition,
  type PlaybackState,
} from './types.js';
import { FilterControls, validateVolume } from './filters.js';

/** Controls one playback session. Local sessions decode into a no-op output. */
export class PlaybackSession<Metadata = unknown> extends RemediaEventEmitter<
  PlaybackSessionEventMap<Metadata>
> {
  readonly #native: NativeRemedia;
  readonly #playbackId: string;
  readonly #onDispose: (() => void) | undefined;
  readonly #terminalGenerations = new Set<number>();
  readonly #metadata = new Map<number, Metadata | undefined>();
  readonly #startedGenerations = new Set<number>();
  readonly hooks: PlaybackHooks<Metadata>;
  readonly filters: FilterControls;
  #input: ReadableInputHandle | undefined;
  #disposed = false;
  #starting = false;
  #playController: AbortController | undefined;
  #signal: AbortSignal | undefined;
  #abortListener: (() => void) | undefined;
  #latestGeneration = 0;
  #guildId: string | undefined;

  /** @internal */
  constructor(
    native: NativeRemedia,
    playbackId: string,
    onDispose?: () => void,
    guildId?: string,
    hooks?: PlaybackHooks<Metadata>,
  ) {
    super();
    this.#native = native;
    this.#playbackId = playbackId;
    this.#onDispose = onDispose;
    this.#guildId = guildId;
    this.hooks = hooks ?? new PlaybackHooks<Metadata>();
    this.filters = new FilterControls(
      () => this.#native.playbackFilterGraph(this.#playbackId) ?? null,
      (graph) => this.#replaceFilterGraph(graph),
      () => this.#replaceFilterGraph(null),
    );
  }

  /** Stable opaque identity for this wrapper, suitable for correlating lifecycle events. */
  get sessionId(): string {
    return this.#playbackId;
  }

  /** @internal Latest native source generation observed by this wrapper. */
  get currentGeneration(): number {
    return this.#latestGeneration;
  }

  get state(): PlaybackState {
    return this.#native.playbackState(this.#playbackId) as PlaybackState;
  }

  get position(): PlaybackPosition {
    const position = this.#native.playbackPosition(this.#playbackId);
    return {
      mediaMs: position.mediaMs ?? null,
      mediaPositionAccuracy:
        position.mediaPositionAccuracy === 'timestamp' ||
        position.mediaPositionAccuracy === 'estimated'
          ? position.mediaPositionAccuracy
          : null,
      submittedMs: position.submittedMs,
      playedMs: position.playedMs ?? null,
    };
  }

  get durationMs(): number | null {
    return this.#native.playbackDurationMs(this.#playbackId) ?? null;
  }

  get error(): string | null {
    return this.#native.playbackError(this.#playbackId) ?? null;
  }

  /** Current normalized linear master gain, from 0.0 (mute) through 2.0. */
  get volume(): number {
    return this.#native.playbackVolume(this.#playbackId);
  }

  /** Set master gain independently of the raw filtergraph. */
  async setVolume(gain: number): Promise<void> {
    this.#assertActive();
    validateVolume(gain);
    const previousVolume = this.volume;
    if (previousVolume === gain) return;
    await this.#native.playbackSetVolume(this.#playbackId, gain);
    this.emit(PlaybackEvent.volumeChanged, {
      ...this.#eventContext(),
      previousVolume,
      volume: gain,
    });
  }

  async play(source: MediaSource, options: PlayOptions<Metadata> = {}): Promise<AudioStreamInfo> {
    this.#assertActive();
    if (this.#starting || isPlaybackActive(this.state)) {
      throw new Error('playback is already active; stop it before starting another source');
    }
    if (options.signal?.aborted) throw abortError(options.signal.reason);

    this.#starting = true;
    const controller = new AbortController();
    let nativeAttempted = false;
    this.#playController = controller;
    this.#signal = options.signal;
    const abortListener = (): void => {
      controller.abort(options.signal?.reason);
      // This native operation is asynchronous and cancellation-aware. It is never invoked from a
      // Rust callback or while the core holds a playback-state lock.
      if (nativeAttempted) void this.#native.stop(this.#playbackId).catch(() => undefined);
    };
    this.#abortListener = abortListener;
    options.signal?.addEventListener('abort', abortListener, { once: true });

    let input: ReadableInputHandle | undefined;
    const previousGeneration = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
    try {
      await this.hooks.runBeforePlay({
        source,
        customMetadata: options.customMetadata,
        signal: controller.signal,
      });
      this.#assertActive();
      if (controller.signal.aborted) throw abortError(controller.signal.reason);
      await this.#releaseInput();

      let nativeSource: NativeMediaSource;
      if ('stream' in source) {
        input = createReadableInput(this.#native, source.stream);
        nativeSource = input.source;
        this.#input = input;
      } else {
        nativeSource = toNativeSource(source);
      }

      if (controller.signal.aborted) throw abortError(controller.signal.reason);
      nativeAttempted = true;
      const info = toStreamInfo(await this.#native.play(this.#playbackId, nativeSource));
      const generation = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
      this.#latestGeneration = generation;
      this.#metadata.set(generation, options.customMetadata);
      this.#boundMetadata(generation);
      this.#startedGenerations.add(generation);
      this.emit(PlaybackEvent.playbackStarted, {
        ...this.#eventContext(generation),
        state: 'playing',
        info,
      });
      this.#starting = false;
      void this.#observeGeneration(generation, input);
      return info;
    } catch (error) {
      if (input && this.#input === input) await this.#releaseInput();
      if (nativeAttempted && !(error instanceof Error && error.name === 'AbortError')) {
        const generation = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
        this.#latestGeneration = generation;
        if (generation > previousGeneration && this.state === 'failed') {
          this.#metadata.set(generation, options.customMetadata);
          this.#emitTerminal('failed', generation, error);
        }
      }
      if (controller.signal.aborted) throw abortError(controller.signal.reason);
      throw error;
    } finally {
      this.#starting = false;
      if (!nativeAttempted || this.state === 'failed' || controller.signal.aborted) {
        this.#clearAbortBinding(controller);
      }
    }
  }

  async pause(): Promise<void> {
    this.#assertActive();
    const previousState = this.state;
    await this.#native.pause(this.#playbackId);
    if (previousState === 'playing' && this.state === 'paused') {
      this.emit(PlaybackEvent.playbackPaused, {
        ...this.#eventContext(),
        previousState,
        state: 'paused',
      });
    }
  }

  async resume(): Promise<void> {
    this.#assertActive();
    const previousState = this.state;
    await this.#native.resume(this.#playbackId);
    if (previousState === 'paused' && this.state === 'playing') {
      this.emit(PlaybackEvent.playbackResumed, {
        ...this.#eventContext(),
        previousState,
        state: 'playing',
      });
    }
  }

  async seek(positionMs: number): Promise<void> {
    this.#assertActive();
    const previousPositionMs = this.position.mediaMs;
    await this.#native.seek(this.#playbackId, positionMs);
    if (previousPositionMs !== positionMs) {
      this.emit(PlaybackEvent.playbackSeeked, {
        ...this.#eventContext(),
        positionMs,
        previousPositionMs,
      });
    }
  }

  async stop(): Promise<void> {
    this.#assertActive();
    const previousState = this.state;
    const generation = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
    try {
      await this.#native.stop(this.#playbackId);
      if (isPlaybackActive(previousState) && this.state === 'stopped') {
        this.#emitTerminal('stopped', generation, undefined, 'stop');
      }
    } finally {
      await this.#releaseInput();
    }
  }

  async wait(): Promise<PlaybackState> {
    this.#assertActive();
    // Capture synchronously so the native blocking waiter stays bound to the source
    // that was active when wait() was called, even if play() replaces it first.
    const generation = this.#native.playbackGeneration(this.#playbackId);
    try {
      return (await this.#native.wait(this.#playbackId, generation)) as PlaybackState;
    } finally {
      await this.#releaseInput();
    }
  }

  /** Stop and release this standalone playback session. */
  async dispose(): Promise<void> {
    if (this.#disposed) return;
    this.#playController?.abort(new Error('playback session disposed'));
    this.#signal?.removeEventListener('abort', this.#abortListener!);
    try {
      await this.#native.disposePlaybackSession(this.#playbackId);
    } finally {
      await this.#releaseInput();
      this.#disposed = true;
      this.hooks.clear();
      this.#metadata.clear();
      this.emit(PlaybackEvent.sessionDisposed, {
        sessionId: this.sessionId,
        ...(this.#guildId === undefined ? {} : { guildId: this.#guildId }),
        nodeId: null,
        generation: this.#latestGeneration,
      });
      this.#onDispose?.();
    }
  }

  /** @internal */
  async releaseInput(): Promise<void> {
    await this.#releaseInput();
  }

  async #replaceFilterGraph(graph: string | null): Promise<void> {
    this.#assertActive();
    const previousGraph = this.filters.graph;
    if (previousGraph === graph) return;
    if (graph === null) await this.#native.playbackClearFilterGraph(this.#playbackId);
    else await this.#native.playbackSetFilterGraph(this.#playbackId, graph);
    this.emit(PlaybackEvent.filterChanged, {
      ...this.#eventContext(),
      previousGraph,
      graph,
    });
  }

  #eventContext(generation?: number) {
    const activeGeneration = generation ?? this.#latestGeneration;
    const context = lifecycleContext(
      this.sessionId,
      activeGeneration,
      this.#metadata.get(activeGeneration),
    );
    return this.#guildId === undefined ? context : { ...context, guildId: this.#guildId };
  }

  async #observeGeneration(
    generation: number,
    input: ReadableInputHandle | undefined,
  ): Promise<void> {
    try {
      const state = (await this.#native.wait(
        this.#playbackId,
        String(generation),
      )) as PlaybackState;
      if (state === 'completed' || state === 'failed' || state === 'stopped') {
        const error = state === 'failed' ? new Error(this.error ?? 'playback failed') : undefined;
        this.#emitTerminal(state, generation, error);
      }
    } catch (error) {
      if (!this.#disposed) {
        // Native disposal can remove the session before this generation-bound
        // wait settles. In that race, reading playbackState also rejects; the
        // disposal lifecycle event is authoritative and no terminal event is
        // needed for the removed session.
        let state: PlaybackState | undefined;
        try {
          state = this.state;
        } catch {
          state = undefined;
        }
        if (state === 'failed') this.#emitTerminal('failed', generation, asError(error));
      }
    } finally {
      this.#metadata.delete(generation);
      this.#startedGenerations.delete(generation);
      if (this.#latestGeneration === generation) {
        this.#clearAbortBinding(this.#playController);
      }
      await this.#releaseInput(input, generation);
    }
  }

  #emitTerminal(
    state: 'completed' | 'stopped' | 'failed',
    generation: number,
    error?: unknown,
    reason?: string,
  ): void {
    if (
      (state !== 'failed' && !this.#startedGenerations.has(generation)) ||
      this.#terminalGenerations.has(generation)
    )
      return;
    this.#terminalGenerations.add(generation);
    while (this.#terminalGenerations.size > 16) {
      const oldest = this.#terminalGenerations.values().next().value;
      if (oldest === undefined) break;
      this.#terminalGenerations.delete(oldest);
    }
    const base = this.#eventContext(generation);
    if (state === 'completed') {
      this.emit(PlaybackEvent.playbackCompleted, { ...base, state, position: this.position });
    } else if (state === 'stopped') {
      this.emit(
        PlaybackEvent.playbackStopped,
        reason ? { ...base, state, reason } : { ...base, state },
      );
    } else {
      this.emit(PlaybackEvent.playbackFailed, { ...base, state, error: asError(error) });
    }
    this.#metadata.delete(generation);
    this.#clearAbortBinding(this.#playController);
  }

  #boundMetadata(generation: number): void {
    while (this.#metadata.size > 4) {
      const oldest = this.#metadata.keys().next().value;
      if (oldest === undefined || oldest === generation) break;
      this.#metadata.delete(oldest);
    }
  }

  #clearAbortBinding(controller: AbortController | undefined): void {
    if (!controller || this.#playController !== controller) return;
    this.#signal?.removeEventListener('abort', this.#abortListener!);
    this.#playController = undefined;
    this.#signal = undefined;
    this.#abortListener = undefined;
  }

  async #releaseInput(expected?: ReadableInputHandle, generation?: number): Promise<void> {
    if (generation !== undefined && this.#latestGeneration !== generation) {
      await expected?.cancel();
      return;
    }
    const input = this.#input;
    if (expected !== undefined && input !== expected) {
      await expected.cancel();
      return;
    }
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

function numericGeneration(value: string): number {
  const generation = Number(value);
  if (!Number.isSafeInteger(generation) || generation < 0) {
    throw new RangeError('native playback generation exceeds the JavaScript safe integer range');
  }
  return generation;
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function abortError(reason: unknown): Error {
  const error = new Error(reason instanceof Error ? reason.message : 'playback was cancelled');
  error.name = 'AbortError';
  return error;
}
