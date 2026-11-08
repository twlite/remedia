import { RemediaEventEmitter } from './events.js';
import { NativeRemedia } from '../native.js';
import type { NativeMediaSource, NativeCrossfadeEvent, NativeStageSnapshot } from '../native.js';
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
  type StageNextOptions,
  type StageSnapshot,
} from './lifecycle.js';
import {
  toStreamInfo,
  type AudioStreamInfo,
  type PlaybackPosition,
  type PlaybackState,
} from './types.js';
import { FilterControls, validateVolume } from './filters.js';

/**
 * Controls one playback session. Local sessions decode into a no-op output.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export class PlaybackSession<Metadata = unknown> extends RemediaEventEmitter<
  PlaybackSessionEventMap<Metadata>
> {
  readonly #native: NativeRemedia;
  readonly #playbackId: string;
  readonly #onDispose: (() => void) | undefined;
  readonly #terminalGenerations = new Set<number>();
  readonly #metadata = new Map<number, Metadata | undefined>();
  readonly #startedGenerations = new Set<number>();
  /** Ordered, awaited local pre-play hooks; listeners are separate from hooks. */
  readonly hooks: PlaybackHooks<Metadata>;
  /** Persistent raw FFmpeg filtergraph controls for this session. */
  readonly filters: FilterControls;
  #input: ReadableInputHandle | undefined;
  #disposed = false;
  #starting = false;
  #playController: AbortController | undefined;
  #signal: AbortSignal | undefined;
  #abortListener: (() => void) | undefined;
  #latestGeneration = 0;
  #guildId: string | undefined;
  #crossfadeEnabled = false;
  #fadeDurationMs = 5000;
  #crossfadeTimer: ReturnType<typeof setInterval> | undefined;
  #stageInput: ReadableInputHandle | undefined;
  #stageEntryId: string | undefined;
  #stageMetadata: Metadata | undefined;
  #outgoingInput: ReadableInputHandle | undefined;

  /**
   * Construct this wrapper with its owning services. Use the owning engine or client factory for application code.
   *
   * @internal
   * @param native - Owning native engine instance; its resources must outlive this wrapper.
   * @param playbackId - Opaque native playback identity owned by this wrapper.
   * @param onDispose - Callback invoked when this wrapper releases its owning registration.
   * @param guildId - Discord guild snowflake used to route the operation.
   * @param hooks - Optional shared pre-play hooks for the owning Discord session.
   */
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

  /** Enable continuous PCM playback before starting a source. */
  configureCrossfade(options: { enabled: true; durationMs?: number; prepareBeforeMs?: number } | false): void {
    this.#assertActive();
    if (options) {
      const durationMs = options.durationMs ?? 5000;
      this.#native.playbackConfigureCrossfade(
        this.#playbackId, true, durationMs, options.prepareBeforeMs ?? 30000,
      );
      this.#fadeDurationMs = durationMs;
      this.#crossfadeEnabled = true;
    } else {
      this.#native.playbackConfigureCrossfade(this.#playbackId, false, 0, 0);
      this.#crossfadeEnabled = false;
    }
  }

  get stagedSource(): StageSnapshot | null {
    const stage = this.#native.playbackStagedSource(this.#playbackId);
    return stage ? this.#normalizeStage(stage) : null;
  }

  async clearStage(): Promise<void> {
    await this.#native.playbackClearStage(this.#playbackId);
    await this.#stageInput?.cancel();
    this.#stageInput = undefined;
    this.#stageEntryId = undefined;
  }

  async stageNext(source: MediaSource, options: StageNextOptions<Metadata>): Promise<StageSnapshot> {
    this.#assertActive();
    if (!this.#crossfadeEnabled) throw new Error('crossfade is disabled for this session');
    if (!options.entryId?.trim()) throw new TypeError('entryId is required');
    if (!Number.isSafeInteger(options.queueRevision) || options.queueRevision < 0) {
      throw new RangeError('queueRevision must be a non-negative safe integer');
    }
    const sourceRevision = options.sourceRevision ?? 0;
    if (!Number.isSafeInteger(sourceRevision) || sourceRevision < 0) {
      throw new RangeError('sourceRevision must be a non-negative safe integer');
    }
    const expectedGeneration = options.expectedGeneration ??
      numericGeneration(this.#native.playbackSourceGeneration(this.#playbackId));
    const existing = this.stagedSource;
    if (existing && existing.expectedGeneration === expectedGeneration &&
        existing.entryId === options.entryId &&
        existing.queueRevision === options.queueRevision &&
        existing.sourceRevision === sourceRevision) return existing;

    let input: ReadableInputHandle | undefined;
    const nativeSource = 'stream' in source
      ? (input = createReadableInput(this.#native, source.stream)).source
      : toNativeSource(source);
    try {
      const result = await this.#native.playbackStageNext(
        this.#playbackId, nativeSource, String(expectedGeneration),
        options.entryId, String(options.queueRevision), String(sourceRevision),
      );
      const previous = this.#stageInput;
      this.#stageInput = input;
      this.#stageEntryId = options.entryId;
      this.#stageMetadata = options.customMetadata;
      if (previous !== input) await previous?.cancel();
      return this.#normalizeStage(result);
    } catch (error) {
      await input?.cancel();
      throw error;
    }
  }

  #normalizeStage(stage: NativeStageSnapshot): StageSnapshot {
    return {
      expectedGeneration: numericGeneration(stage.expectedGeneration),
      entryId: stage.entryId,
      queueRevision: numericGeneration(stage.queueRevision),
      sourceRevision: numericGeneration(stage.sourceRevision),
      state: stage.state as StageSnapshot['state'],
      bufferedMs: stage.bufferedMs,
      info: stage.info ? toStreamInfo(stage.info) : null,
      error: stage.error ?? null,
    };
  }

  /** Current playback lifecycle state. */
  get state(): PlaybackState {
    return this.#native.playbackState(this.#playbackId) as PlaybackState;
  }

  /** Latest source and output timeline measurements in milliseconds. */
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

  /** Source duration in milliseconds, or null when unknown. */
  get durationMs(): number | null {
    return this.#native.playbackDurationMs(this.#playbackId) ?? null;
  }

  /** Most recent playback or voice failure message, or null when no error is available. */
  get error(): string | null {
    return this.#native.playbackError(this.#playbackId) ?? null;
  }

  /** Current normalized linear master gain, from 0.0 (mute) through 2.0. */
  get volume(): number {
    return this.#native.playbackVolume(this.#playbackId);
  }

  /**
   * Set master gain independently of the raw filtergraph.
   * @param gain - Finite linear master gain from 0 through 2; 1 leaves amplitude unchanged.
   * @returns Resolves after the gain is stored or applied; rejects for non-finite gain or values outside 0 through 2.
   * @throws {RangeError} If gain is non-finite or outside the inclusive range 0 through 2.
   */
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

  /**
   * Open one media source and begin a new playback generation using the persistent session configuration. Stop active playback before starting another source.
   * @param source - Encoded media input; Buffer and caller-owned binary Readable inputs require embedded mode.
   * @param options - Optional metadata for this generation and a cancellation signal.
   * @returns Selected stream metadata once startup succeeds; rejects if hooks, input opening, or playback startup fail.
   */
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
      const workerGeneration = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
      const generation = numericGeneration(this.#native.playbackSourceGeneration(this.#playbackId));
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
      if (this.#crossfadeEnabled) this.#startCrossfadePoll();
      void this.#observeGeneration(workerGeneration, input);
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

  /**
   * Pause the current playback without replacing its decoder or source generation.
   * @returns Resolves when the backend accepts the pause request.
   */
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

  /**
   * Resume paused playback on its existing source generation.
   * @returns Resolves when the backend accepts the resume request.
   */
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

  /**
   * Seek on the original media timeline. Binary Readable inputs support forward-only
   * seeking by decoding and discarding skipped audio; backwards seeking rejects.
   * Large forward jumps may wait for the producer to supply the skipped bytes.
   * @param positionMs - Non-negative finite position on the original media timeline, in milliseconds.
   * @returns Resolves after the backend accepts the seek; rejects for invalid positions or unsupported inputs.
   */
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

  /**
   * Stop the active playback generation and release its input without leaving the voice channel.
   * @returns Resolves after the backend stops the active source.
   */
  async stop(): Promise<void> {
    this.#assertActive();
    const previousState = this.state;
    const generation = this.#latestGeneration;
    try {
      await this.#native.stop(this.#playbackId);
      if (isPlaybackActive(previousState) && this.state === 'stopped') {
        this.#emitTerminal('stopped', generation, undefined, 'stop');
      }
    } finally {
      await this.#releaseInput();
    }
  }

  /**
   * Wait for the current playback generation to reach a terminal state.
   * @returns The terminal playback state for the generation captured by this wait.
   */
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

  /**
   * Stop and release this standalone playback session.
   * @returns Resolves after disposal; rejects if resource cleanup fails.
   */
  async dispose(): Promise<void> {
    if (this.#disposed) return;
    this.#playController?.abort(new Error('playback session disposed'));
    this.#signal?.removeEventListener('abort', this.#abortListener!);
    try {
      await this.#native.disposePlaybackSession(this.#playbackId);
    } finally {
      await this.#releaseInput();
      this.#disposed = true;
      this.#stopCrossfadePoll();
      await this.#stageInput?.cancel();
      await this.#outgoingInput?.cancel();
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

  /**
   * Detach and cancel this wrapper's bounded input pump without destroying the caller-owned Readable.
   *
   * @internal
   * @returns Resolves after the input pump has released its listeners and native queue.
   */
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
        this.#drainCrossfadeEvents();
        const error = state === 'failed' ? new Error(this.error ?? 'playback failed') : undefined;
        this.#emitTerminal(state, this.#latestGeneration, error);
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
        if (state === 'failed') this.#emitTerminal('failed', this.#latestGeneration, asError(error));
      }
    } finally {
      this.#stopCrossfadePoll();
      this.#clearAbortBinding(this.#playController);
      await this.#outgoingInput?.cancel();
      this.#outgoingInput = undefined;
      await this.#stageInput?.cancel();
      this.#stageInput = undefined;
      await this.#releaseInput();
      if (input !== this.#input) await input?.cancel();
    }
  }

  #emitTerminal(
    state: 'completed' | 'stopped' | 'failed',
    generation: number,
    error?: unknown,
    reason?: string,
    continuation?: { type: 'promoted'; generation: number; entryId: string },
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
      this.emit(PlaybackEvent.playbackCompleted, {
        ...base, state, position: this.position,
        ...(continuation ? { continuation } : {}),
      });
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
