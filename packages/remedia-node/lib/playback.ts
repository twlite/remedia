import { Readable } from 'node:stream';
import { NativeAudioOutput } from '../native.js';
import type { AudioReadable, OpusPacket, StreamOutputOptions } from './stream-output.js';
import { RemediaEventEmitter } from './events.js';
import { NativeRemedia } from './native-binding.js';
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
  type CrossfadeCurve,
  type CrossfadeOptions,
  type StageSnapshot,
} from './lifecycle.js';
import {
  toStreamInfo,
  type AudioStreamInfo,
  type PlaybackPosition,
  type PlaybackState,
} from './types.js';
import { FilterControls, validateVolume } from './filters.js';

/** Crossfade curves accepted by the native engine. */
const CROSSFADE_CURVES: readonly CrossfadeCurve[] = ['equalPower', 'adaptive'];

/**
 * Most generations remembered for lifecycle deduplication. At most a few generations are live at
 * once (the active source and one fading out), so this only has to outlast late duplicates.
 *
 * @internal
 */
export const GENERATION_HISTORY_LIMIT = 16;

/** Attempts to read native state while the crossfade event queue is still changing. */
const RECONCILE_ATTEMPTS = 5;

/**
 * Extra time, beyond the fade length, before an input whose fade state is unknown is released
 * after the native event queue overflowed.
 */
const OVERFLOW_RELEASE_MARGIN_MS = 2000;

/** Identifies one staged queue occurrence, matching the identity the native engine reports. */
interface StageKeyParts {
  generation: number;
  entryId: string;
  queueRevision: number;
  sourceRevision: number;
}

/** Local bookkeeping for one staged occurrence and the input that feeds it. */
interface StageRecord<Metadata> {
  input: ReadableInputHandle | undefined;
  metadata: Metadata | undefined;
  identity: StageKeyParts;
  /** True until the native staging call for this entry settles; its stage may not exist yet. */
  pending: boolean;
}

/**
 * One `play()` call and the resources it owns until a newer `play()` replaces it. Cleanup that
 * runs after an asynchronous gap (the lifecycle observer, `wait()`, `stop()`) compares its run with
 * the session's current one first, so a late callback never releases what a replacement owns.
 */
interface PlaybackRun {
  /** Native stream output owned by this run, if streaming. */
  output?: NativeAudioOutput | undefined;
  /** Cancels this run's startup and, through its abort binding, its playback. */
  readonly controller: AbortController;
  /** The input the run started with. Promoted stage inputs replace it as the session's input. */
  input: ReadableInputHandle | undefined;
  /** Generation that was current when a newer run replaced this one, for its late terminal event. */
  finalGeneration?: number;
}

/** Native playback state read together, used to repair local state after lost events. */
interface NativeSnapshot {
  generation: number;
  staged: StageKeyParts | null;
  active: StageKeyParts | null;
}

function stageKey(parts: StageKeyParts): string {
  return JSON.stringify([
    parts.generation,
    parts.entryId,
    parts.queueRevision,
    parts.sourceRevision,
  ]);
}

/**
 * Add `generation` as the most recent entry, evicting the oldest beyond the history limit.
 *
 * @internal
 */
export function rememberGeneration(generations: Set<number>, generation: number): void {
  generations.delete(generation);
  generations.add(generation);
  while (generations.size > GENERATION_HISTORY_LIMIT) {
    const oldest = generations.values().next().value;
    if (oldest === undefined) break;
    generations.delete(oldest);
  }
}

/**
 * Controls one playback session. Local sessions decode into a no-op output.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
/** Options for {@link PlaybackSession.seek}. */
export interface SeekOptions {
  /** Cancels a seek that has not taken effect yet. */
  signal?: AbortSignal;
}

/** Settings for a local playback session. Every setting is off unless enabled. */
export interface PlaybackSessionOptions {
  /**
   * Replace the active source when its decoder cannot seek, instead of rejecting the seek. See
   * `PlaybackSession.configureFallbackSeeking()`. Defaults to false.
   */
  enableFallbackSeeking?: boolean;
}

export class PlaybackSession<Metadata = unknown> extends RemediaEventEmitter<
  PlaybackSessionEventMap<Metadata>
> {
  readonly #native: NativeRemedia;
  readonly #playbackId: string;
  readonly #onDispose: (() => void) | undefined;
  /** Most recent generations that already emitted a terminal event, bounded for deduplication. */
  readonly #terminalGenerations = new Set<number>();
  readonly #metadata = new Map<number, Metadata | undefined>();
  /** Most recent generations started but not yet terminal, bounded; see rememberGeneration. */
  readonly #startedGenerations = new Set<number>();
  /** Ordered, awaited local pre-play hooks; listeners are separate from hooks. */
  readonly hooks: PlaybackHooks<Metadata>;
  /** Persistent raw FFmpeg filtergraph controls for this session. */
  readonly filters: FilterControls;
  #input: ReadableInputHandle | undefined;
  /** The latest `play()` call; its observer and callers may release `#input`, older ones may not. */
  #run: PlaybackRun | undefined;
  #disposed = false;
  #starting = false;
  #playController: AbortController | undefined;
  #signal: AbortSignal | undefined;
  #abortListener: (() => void) | undefined;
  #latestGeneration = 0;
  #guildId: string | undefined;
  #crossfadeEnabled = false;
  #seekSerial = 0;
  #fadeDurationMs = 5000;
  /** Token of the running native notification loop; replacing or clearing it stops that loop. */
  #eventLoop: object | undefined;
  #stageInput: ReadableInputHandle | undefined;
  #stageEntryId: string | undefined;
  #stageWork: Promise<void> = Promise.resolve();
  readonly #stages = new Map<string, StageRecord<Metadata>>();
  #outgoingInput: ReadableInputHandle | undefined;
  /** Key of the promoted stage that feeds the active source; undefined for a plain `play()`. */
  #activeStageKey: string | undefined;
  /** Key of the entry fading out while a transition is in progress. */
  #outgoingStageKey: string | undefined;
  /** Releases the outgoing input once a fade of unknown progress has certainly finished. */
  #outgoingRelease: ReturnType<typeof setTimeout> | undefined;
  /** Inputs of stages that vanished with lost events, each released by its own timer. */
  readonly #retiring = new Map<ReadableInputHandle, ReturnType<typeof setTimeout>>();

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
  /**
   * Configure overlapping PCM playback while idle.
   * @param options - Explicit opt-in settings, or false to retain ordinary playback.
   * `curve` selects `equalPower` (default) or `adaptive`.
   * @returns Nothing; throws if settings are invalid or playback is active.
   */
  configureCrossfade(options: CrossfadeOptions | false): void {
    this.#assertActive();
    if (options) {
      if (
        options.enabled !== true ||
        (options.curve !== undefined && !CROSSFADE_CURVES.includes(options.curve))
      ) {
        throw new TypeError('crossfade requires enabled: true and an equalPower or adaptive curve');
      }
      const durationMs = options.durationMs ?? 5000;
      this.#native.playbackConfigureCrossfade(
        this.#playbackId,
        true,
        durationMs,
        options.prepareBeforeMs ?? 30000,
        options.curve ?? 'equalPower',
      );
      this.#fadeDurationMs = durationMs;
      this.#crossfadeEnabled = true;
    } else {
      this.#native.playbackConfigureCrossfade(this.#playbackId, false, 0, 0);
      this.#crossfadeEnabled = false;
    }
  }

  /**
   * Whether fallback seeking is enabled; it is off by default.
   * @returns True when a seek the active decoder cannot perform may be served by a replacement source.
   */
  get fallbackSeeking(): boolean {
    return this.#native.playbackFallbackSeeking(this.#playbackId);
  }

  /**
   * Allow or forbid fallback seeking. It can be changed at any time and applies to the next seek.
   *
   * Some sources cannot reposition their decoder, such as an HTTP server that ignores range
   * requests. A seek on one normally rejects as unsupported. With this enabled, the session keeps
   * playing while it reopens the same track, decodes forward to the target plus the playback that
   * happened meanwhile, and swaps the source with a 5 ms blend. `seek()` resolves when the swap
   * has happened. A one-shot byte stream still rejects, and so does a source that fails to reopen;
   * the current playback is never interrupted by a failed or superseded fallback.
   * @param enabled - True to allow fallback seeking; false to restore the unsupported-seek rejection.
   * @returns Nothing; throws if the session is disposed or `enabled` is not a boolean.
   */
  configureFallbackSeeking(enabled: boolean): void {
    this.#assertActive();
    if (typeof enabled !== 'boolean') throw new TypeError('enabled must be a boolean');
    this.#native.playbackConfigureFallbackSeeking(this.#playbackId, enabled);
  }

  /** Current preparation snapshot, or null when no stage is pending. */
  get stagedSource(): StageSnapshot | null {
    const stage = this.#native.playbackStagedSource(this.#playbackId);
    return stage ? this.#normalizeStage(stage) : null;
  }

  /** Cancel the queued stage and release its input. @returns Resolves after cancellation. */
  async clearStage(): Promise<void> {
    const work = this.#stageWork.then(() => this.#clearStage());
    this.#stageWork = work.then(
      () => {},
      () => {},
    );
    return work;
  }

  async #clearStage(): Promise<void> {
    await this.#native.playbackClearStage(this.#playbackId);
    this.#drainCrossfadeEvents();
    await this.#stageInput?.cancel();
    this.#stageInput = undefined;
    this.#stageEntryId = undefined;
  }

  /**
   * Prepare one upcoming source without restarting playback or its output.
   * @param source - Fresh media input; Readables and buffers require embedded execution.
   * @param options - Queue occurrence, revision fencing, expiry and locally retained metadata.
   * @returns Preparation snapshot; rejects disabled sessions, stale identities or invalid inputs.
   */
  async stageNext(
    source: MediaSource,
    options: StageNextOptions<Metadata>,
  ): Promise<StageSnapshot> {
    const work = this.#stageWork.then(() => this.#submitStage(source, options));
    this.#stageWork = work.then(
      () => {},
      () => {},
    );
    return work;
  }

  async #submitStage(
    source: MediaSource,
    options: StageNextOptions<Metadata>,
  ): Promise<StageSnapshot> {
    this.#assertActive();
    this.#drainCrossfadeEvents();
    if (!this.#crossfadeEnabled) throw new Error('crossfade is disabled for this session');
    if (!options.entryId?.trim()) throw new TypeError('entryId is required');
    if (!Number.isSafeInteger(options.queueRevision) || options.queueRevision < 0) {
      throw new RangeError('queueRevision must be a non-negative safe integer');
    }
    const sourceRevision = options.sourceRevision ?? 0;
    if (!Number.isSafeInteger(sourceRevision) || sourceRevision < 0) {
      throw new RangeError('sourceRevision must be a non-negative safe integer');
    }
    if (
      options.expiresAtMs !== undefined &&
      (!Number.isSafeInteger(options.expiresAtMs) || options.expiresAtMs < 0)
    ) {
      throw new RangeError('expiresAtMs must be a non-negative safe integer');
    }
    const expectedGeneration =
      options.expectedGeneration ??
      numericGeneration(this.#native.playbackSourceGeneration(this.#playbackId));
    const existing = this.stagedSource;
    if (
      existing &&
      existing.expectedGeneration === expectedGeneration &&
      existing.entryId === options.entryId &&
      existing.queueRevision === options.queueRevision &&
      existing.sourceRevision === sourceRevision
    )
      return existing;

    let input: ReadableInputHandle | undefined;
    const nativeSource =
      'stream' in source
        ? (input = createReadableInput(this.#native, source.stream)).source
        : toNativeSource(source);
    const key = stageKey({
      generation: expectedGeneration,
      entryId: options.entryId,
      queueRevision: options.queueRevision,
      sourceRevision,
    });
    const record: StageRecord<Metadata> = {
      input,
      metadata: options.customMetadata,
      identity: {
        generation: expectedGeneration,
        entryId: options.entryId,
        queueRevision: options.queueRevision,
        sourceRevision,
      },
      pending: true,
    };
    this.#stages.set(key, record);
    try {
      const result = await this.#native.playbackStageNext(
        this.#playbackId,
        nativeSource,
        String(expectedGeneration),
        options.entryId,
        String(options.queueRevision),
        String(sourceRevision),
        options.expiresAtMs === undefined ? undefined : String(options.expiresAtMs),
      );
      this.#drainCrossfadeEvents();
      if (this.#stages.has(key)) {
        this.#stageInput = input;
        this.#stageEntryId = key;
      }
      return this.#normalizeStage(result);
    } catch (error) {
      this.#stages.delete(key);
      await input?.cancel();
      throw error;
    } finally {
      record.pending = false;
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
    return this.#play(source, options);
  }

  /** Start packet-preserving Opus output on this standalone session.
   * @param source - Encoded media source owned by this playback generation.
   * @param options - Opus format, pacing, encoder controls, metadata and cancellation.
   * @returns Object-mode Readable; destruction cancels only its associated playback.
   */
  playStream(
    source: MediaSource,
    options: StreamOutputOptions & PlayOptions<Metadata> & { format: 'opus' },
  ): Promise<AudioReadable<OpusPacket>>;
  /** Start normalized PCM output on this standalone session.
   * @param source - Encoded media source owned by this playback generation.
   * @param options - PCM format, pacing, metadata and cancellation.
   * @returns Binary Readable carrying interleaved 48 kHz stereo PCM.
   */
  playStream(
    source: MediaSource,
    options: StreamOutputOptions & PlayOptions<Metadata> & { format: 's16le' | 'f32le' },
  ): Promise<AudioReadable<Buffer>>;
  /** Start streaming with a dynamically selected format.
   * @param source - Encoded media input.
   * @param options - Output and playback configuration; unpaced by default.
   * @returns A bounded Node Readable. Native queued data is flushed on seek; data
   * already handed to JavaScript or a downstream consumer cannot be retracted.
   */
  playStream(
    source: MediaSource,
    options: StreamOutputOptions & PlayOptions<Metadata>,
  ): Promise<AudioReadable<Buffer | OpusPacket>>;
  async playStream(
    source: MediaSource,
    options: StreamOutputOptions & PlayOptions<Metadata>,
  ): Promise<AudioReadable<Buffer | OpusPacket>> {
    const output = new NativeAudioOutput(
      options.format,
      options.pacing ?? 'unpaced',
      options.opus?.bitrate,
      options.opus?.complexity,
      options.opus?.vbr,
    );
    try {
      await this.#play(source, options, output);
    } catch (error) {
      output.cancel();
      throw error;
    }
    let ended = false;
    const stopRun = (): Promise<void> =>
      !this.#disposed &&
      this.#run?.output === output &&
      !this.#run.controller.signal.aborted &&
      this.#hasActivePlayback()
        ? this.stop()
        : Promise.resolve();
    let reading = false;
    const stream = new Readable({
      objectMode: options.format === 'opus',
      highWaterMark: options.format === 'opus' ? 1 : 16 * 1024,
      read() {
        if (reading) return;
        reading = true;
        void output.read().then(
          (chunk) => {
            reading = false;
            if (!chunk) ended = true;
            if (!this.destroyed)
              this.push(chunk ? (options.format === 'opus' ? chunk : chunk.data) : null);
          },
          (error: unknown) => {
            reading = false;
            this.destroy(asError(error));
          },
        );
      },
      destroy(error, done) {
        void (ended ? Promise.resolve() : stopRun()).then(
          () => {
            output.cancel();
            done(error);
          },
          (failure: unknown) => {
            output.cancel();
            done(error ?? asError(failure));
          },
        );
      },
    });
    return stream;
  }

  async #play(
    source: MediaSource,
    options: PlayOptions<Metadata>,
    output?: NativeAudioOutput,
  ): Promise<AudioStreamInfo> {
    this.#assertActive();
    if (this.#starting || isPlaybackActive(this.state)) {
      throw new Error('playback is already active; stop it before starting another source');
    }
    if (options.signal?.aborted) throw abortError(options.signal.reason);

    // The previous playback has ended. If its observer has not run yet, its abort listener is
    // still attached, and aborting that signal later would stop this playback instead.
    this.#clearAbortBinding(this.#playController);
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
    let run: PlaybackRun | undefined;
    const previousGeneration = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
    try {
      await this.hooks.runBeforePlay({
        source,
        customMetadata: options.customMetadata,
        signal: controller.signal,
      });
      this.#assertActive();
      if (controller.signal.aborted) throw abortError(controller.signal.reason);

      // From here this call owns the session's input. Replacing the run before the first await
      // makes every pending observer, wait() and stop() of the previous run a bystander, and the
      // previous run's inputs and stages are released here instead of by those late callbacks.
      if (this.#run) this.#run.finalGeneration = this.#latestGeneration;
      run = { controller, input: undefined, output };
      this.#run = run;
      await this.#releaseInput();
      await this.#releaseStagedResources();

      let nativeSource: NativeMediaSource;
      if ('stream' in source) {
        input = createReadableInput(this.#native, source.stream);
        run.input = input;
        nativeSource = input.source;
        this.#input = input;
      } else {
        nativeSource = toNativeSource(source);
      }

      if (controller.signal.aborted) throw abortError(controller.signal.reason);
      nativeAttempted = true;
      const info = toStreamInfo(await this.#native.play(this.#playbackId, nativeSource, output));
      const workerGeneration = numericGeneration(this.#native.playbackGeneration(this.#playbackId));
      const generation = numericGeneration(this.#native.playbackSourceGeneration(this.#playbackId));
      this.#latestGeneration = generation;
      this.#metadata.set(generation, options.customMetadata);
      this.#boundMetadata(generation);
      rememberGeneration(this.#startedGenerations, generation);
      this.#activeStageKey = undefined;
      this.#outgoingStageKey = undefined;
      this.emit(PlaybackEvent.playbackStarted, {
        ...this.#eventContext(generation),
        state: 'playing',
        info,
      });
      this.#starting = false;
      this.#startEventLoop();
      void this.#observeGeneration(workerGeneration, run);
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
   *
   * With `configureFallbackSeeking()` enabled, a URL or file whose decoder cannot seek is
   * served by a replacement source instead: the promise resolves once the swap has happened,
   * which can take seconds while the current source keeps playing. It rejects when a newer seek
   * replaced this one, when the session stopped, or when the track could not be reopened.
   *
   * The `playbackSeeked` event reports the position the audio actually continues from. A
   * fallback seek lands on the requested position plus the source time that elapsed while its
   * replacement was prepared, so a seek to 90 s that took three seconds resumes near 93 s.
   * @param positionMs - Non-negative finite position on the original media timeline, in milliseconds.
   * @param options - Optional cancellation. Aborting `signal` cancels a seek that has not taken
   * effect yet, so it can never apply later, and rejects with the signal's reason. If the seek
   * was already being applied it completes normally.
   * @returns Resolves after the backend accepts the seek; rejects for invalid positions or unsupported inputs.
   */
  async seek(positionMs: number, options: SeekOptions = {}): Promise<void> {
    this.#assertActive();
    const { signal } = options;
    signal?.throwIfAborted();
    const previousPositionMs = this.position.mediaMs;
    // Only the latest seek of this session can be cancelled, so a cancel for an older call that
    // a newer one replaced must not reach the newer one.
    const serial = ++this.#seekSerial;
    const abort = (): void => {
      if (this.#seekSerial === serial) this.#native.playbackCancelSeek(this.#playbackId);
    };
    signal?.addEventListener('abort', abort, { once: true });
    let outcome;
    try {
      outcome = await this.#native.seek(this.#playbackId, positionMs);
    } catch (error) {
      if (signal?.aborted) throw signal.reason;
      throw error;
    } finally {
      signal?.removeEventListener('abort', abort);
    }
    this.#drainCrossfadeEvents();
    const appliedMs = Math.round(outcome.appliedMs);
    if (previousPositionMs !== appliedMs || this.#crossfadeEnabled) {
      this.emit(PlaybackEvent.playbackSeeked, {
        ...this.#eventContext(),
        positionMs: appliedMs,
        requestedPositionMs: Math.round(outcome.requestedMs),
        fallback: outcome.replacedSource,
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
    const run = this.#run;
    try {
      await this.#native.stop(this.#playbackId);
      if (isPlaybackActive(previousState) && this.state === 'stopped') {
        this.#emitTerminal('stopped', generation, undefined, 'stop');
      }
    } finally {
      // A play() that started while this stop was settling owns the input and the abort binding.
      if (run) this.#clearAbortBinding(run.controller);
      if (this.#run === run) await this.#releaseInput();
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
    const run = this.#run;
    try {
      return (await this.#native.wait(this.#playbackId, generation)) as PlaybackState;
    } finally {
      // Only the playback this wait observed may be released. If a newer one replaced it while
      // the wait was pending, the newer playback owns the input.
      if (this.#run === run) await this.#releaseInput();
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
      this.#stopEventLoop();
      await this.#releaseStagedResources();
      this.hooks.clear();
      this.#metadata.clear();
      this.#startedGenerations.clear();
      this.#terminalGenerations.clear();
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

  async #observeGeneration(generation: number, run: PlaybackRun): Promise<void> {
    // A newer play() makes this observer late. It still reports how its own playback ended, as
    // that generation, but it must not touch state that now belongs to the replacement.
    const terminalGeneration = (): number =>
      this.#run === run ? this.#latestGeneration : (run.finalGeneration ?? this.#latestGeneration);
    try {
      const state = (await this.#native.wait(
        this.#playbackId,
        String(generation),
      )) as PlaybackState;
      if (state === 'completed' || state === 'failed' || state === 'stopped') {
        this.#drainCrossfadeEvents();
        const error = state === 'failed' ? new Error(this.error ?? 'playback failed') : undefined;
        this.#emitTerminal(state, terminalGeneration(), error);
      }
    } catch (error) {
      if (!this.#disposed && this.#run === run) {
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
        if (state === 'failed')
          this.#emitTerminal('failed', this.#latestGeneration, asError(error));
      }
    } finally {
      this.#clearAbortBinding(run.controller);
      // This wait also ends when a newer playback replaces the source. That playback then owns
      // the staged inputs and the notification loop, so they are released only when none is active.
      if (!this.#hasActivePlayback()) {
        this.#stopEventLoop();
        await this.#releaseStagedResources();
      }
      // The session's input belongs to the latest run, including any stage promoted into it. A
      // replaced run's own input was already released by its replacement.
      if (this.#run === run) await this.#releaseInput();
      await run.input?.cancel();
    }
  }

  #hasActivePlayback(): boolean {
    if (this.#starting) return true;
    try {
      return isPlaybackActive(this.state);
    } catch {
      // The native session is gone, so nothing is playing.
      return false;
    }
  }

  /** Cancel every staged, outgoing and prepared input and forget which stage is active. */
  async #releaseStagedResources(): Promise<void> {
    this.#clearOutgoingRelease();
    for (const timer of this.#retiring.values()) clearTimeout(timer);
    const inputs = new Set<ReadableInputHandle | undefined>([
      this.#outgoingInput,
      this.#stageInput,
      ...this.#retiring.keys(),
      ...[...this.#stages.values()].map((stage) => stage.input),
    ]);
    this.#retiring.clear();
    this.#outgoingInput = undefined;
    this.#stageInput = undefined;
    this.#stageEntryId = undefined;
    this.#stages.clear();
    this.#activeStageKey = undefined;
    this.#outgoingStageKey = undefined;
    for (const staged of inputs) await staged?.cancel();
  }

  /**
   * Start the native notification loop for the active playback. It replaces the former fixed
   * interval: the loop sleeps in native code until crossfade events or a terminal change are
   * published, so an idle or paused session wakes nothing.
   */
  #startEventLoop(): void {
    if (this.#eventLoop || !this.#crossfadeEnabled || this.#disposed) return;
    const loop = {};
    this.#eventLoop = loop;
    void this.#runEventLoop(loop);
  }

  #stopEventLoop(): void {
    // The native wait resolves when the session stops or is disposed; the loop then sees that
    // its token was replaced and exits without touching newer state.
    this.#eventLoop = undefined;
  }

  async #runEventLoop(loop: object): Promise<void> {
    try {
      while (this.#eventLoop === loop && !this.#disposed) {
        // Read the revision before draining. Anything published after this read changes the
        // revision and resolves the wait below, so no notification can fall between the drain and
        // the registration of the wait.
        const revision = this.#native.playbackEventsRevision(this.#playbackId);
        try {
          this.#drainCrossfadeEvents();
        } catch (error) {
          // Listener failures are isolated by emit(). Anything else is a genuine bookkeeping
          // failure; it must not end notifications for the rest of the playback, so surface it
          // where a timer callback's error would have surfaced, as an uncaught exception.
          queueMicrotask(() => {
            throw error;
          });
        }
        if (!isPlaybackActive(this.state)) break;
        await this.#native.playbackWaitEvents(this.#playbackId, revision);
      }
    } catch {
      // The native session was disposed or the engine closed. Terminal lifecycle events come from
      // #observeGeneration, which owns them.
    } finally {
      if (this.#eventLoop === loop) this.#eventLoop = undefined;
    }
  }

  #drainCrossfadeEvents(): void {
    if (!this.#crossfadeEnabled || this.#disposed) return;
    let events: NativeCrossfadeEvent[];
    try {
      events = this.#native.playbackTakeCrossfadeEvents(this.#playbackId);
    } catch {
      return;
    }
    if (events.some((event) => event.kind === 'events_dropped')) {
      // Older events were evicted, so the history is incomplete and applying the rest of this
      // batch could corrupt local state. Rebuild that state from the engine instead.
      this.#reconcileAfterOverflow();
      return;
    }
    // Local bookkeeping must stay consistent with the engine, so one event that fails to apply
    // cannot skip the rest of the batch. The first error is rethrown once all are applied.
    let failure: { error: unknown } | undefined;
    for (const event of events) {
      try {
        this.#applyCrossfadeEvent(event);
      } catch (error) {
        failure ??= { error };
      }
    }
    if (failure) throw failure.error;
  }

  #readNativeSnapshot(): NativeSnapshot {
    const toKey = (identity: {
      expectedGeneration: string;
      entryId: string;
      queueRevision: string;
      sourceRevision: string;
    }): StageKeyParts => ({
      generation: numericGeneration(identity.expectedGeneration),
      entryId: identity.entryId,
      queueRevision: numericGeneration(identity.queueRevision),
      sourceRevision: numericGeneration(identity.sourceRevision),
    });
    const staged = this.#native.playbackStagedSource(this.#playbackId);
    const active = this.#native.playbackActiveStage(this.#playbackId);

    return {
      generation: numericGeneration(this.#native.playbackSourceGeneration(this.#playbackId)),
      staged: staged ? toKey(staged) : null,
      active: active ? toKey(active) : null,
    };
  }

  /**
   * Repair local state after the bounded native event queue overflowed.
   *
   * The engine's current generation, staged entry and promoted entry are authoritative. Events
   * queued before the read are discarded, because the snapshot already reflects them. Replaying
   * them on top of it would apply a transition twice. The read is repeated while the queue keeps
   * changing, so an event cannot be discarded without being reflected in the snapshot. Events that
   * race past this point are applied normally; their handlers are idempotent against this state.
   *
   * Lifecycle events that were lost, such as `playbackStarted` for a promotion, are not replayed.
   * Staged entries that are no longer staged or active are reported through `stageInvalidated`.
   */
  #reconcileAfterOverflow(): void {
    let snapshot: NativeSnapshot;
    try {
      for (let attempt = 0; ; attempt++) {
        const before = this.#native.playbackEventsRevision(this.#playbackId);
        this.#native.playbackTakeCrossfadeEvents(this.#playbackId);
        snapshot = this.#readNativeSnapshot();
        const after = this.#native.playbackEventsRevision(this.#playbackId);
        if (after === before || attempt + 1 >= RECONCILE_ATTEMPTS) break;
      }
    } catch {
      // The native session was disposed while reconciling; there is no state left to repair.
      return;
    }

    const activeKey = snapshot.active ? stageKey(snapshot.active) : undefined;
    const stagedKey = snapshot.staged ? stageKey(snapshot.staged) : undefined;
    const previousGeneration = this.#latestGeneration;
    let adopted: StageRecord<Metadata> | undefined;

    if (activeKey !== this.#activeStageKey) {
      adopted = activeKey === undefined ? undefined : this.#stages.get(activeKey);
      if (adopted && activeKey !== undefined) {
        // The promotion was lost. The staged input now feeds the active source and the previous
        // input is fading out, or already silent.
        void this.#outgoingInput?.cancel();
        this.#clearOutgoingRelease();
        this.#outgoingInput = this.#input;
        this.#input = adopted.input;
        this.#outgoingStageKey = this.#activeStageKey;
        this.#activeStageKey = activeKey;
        this.#stages.delete(activeKey);
        if (this.#stageEntryId === activeKey) {
          this.#stageInput = undefined;
          this.#stageEntryId = undefined;
        }
        this.#metadata.set(snapshot.generation, adopted.metadata);
      } else if (activeKey === this.#outgoingStageKey) {
        // The transition was cancelled and the previous source is active again.
        const abandoned = this.#input;
        this.#input = this.#outgoingInput;
        this.#outgoingInput = undefined;
        void abandoned?.cancel();
        this.#activeStageKey = activeKey;
        this.#outgoingStageKey = undefined;
      } else {
        // Only the identity is tracked: the engine's active entry has no local record, so no
        // local input can be attributed to it and none is touched.
        this.#activeStageKey = activeKey;
      }
    }

    if (snapshot.generation !== previousGeneration) {
      if (!adopted) {
        this.#metadata.set(snapshot.generation, this.#metadata.get(previousGeneration));
      }
      this.#latestGeneration = snapshot.generation;
    }
    this.#boundMetadata(snapshot.generation);
    rememberGeneration(this.#startedGenerations, snapshot.generation);

    const gone: Array<StageRecord<Metadata>> = [];
    for (const [key, record] of [...this.#stages]) {
      if (key === stagedKey || record.pending) continue;
      this.#stages.delete(key);
      if (record.input) this.#retireAfterFade(record.input);
      gone.push(record);
    }
    if (this.#stageEntryId !== undefined && !this.#stages.has(this.#stageEntryId)) {
      this.#stageInput = undefined;
      this.#stageEntryId = undefined;
    }
    this.#scheduleOutgoingRelease();

    // Listeners run last, against settled state.
    let failure: { error: unknown } | undefined;
    for (const record of gone) {
      try {
        this.emit(PlaybackEvent.stageInvalidated, {
          ...this.#eventContext(record.identity.generation),
          entryId: record.identity.entryId,
          queueRevision: record.identity.queueRevision,
          sourceRevision: record.identity.sourceRevision,
        });
      } catch (error) {
        failure ??= { error };
      }
    }
    if (failure) throw failure.error;
  }

  /**
   * Release an input that a lost event may have left feeding an audible fade. Its fade cannot be
   * longer than the configured one, so it is released shortly after that.
   */
  #retireAfterFade(input: ReadableInputHandle): void {
    if (this.#retiring.has(input)) return;
    const timer = setTimeout(() => {
      this.#retiring.delete(input);
      void input.cancel();
    }, this.#fadeDurationMs + OVERFLOW_RELEASE_MARGIN_MS);
    timer.unref?.();
    this.#retiring.set(input, timer);
  }

  /**
   * After an overflow the outgoing input's fade may or may not have finished, and its
   * `transitionCompleted` event may have been lost. Release it once the fade certainly has. A
   * newer transition or a cancellation replaces `#outgoingInput`, which makes this a no-op.
   */
  #scheduleOutgoingRelease(): void {
    if (this.#outgoingRelease) clearTimeout(this.#outgoingRelease);
    this.#outgoingRelease = undefined;
    const outgoing = this.#outgoingInput;
    if (!outgoing) return;
    this.#outgoingRelease = setTimeout(() => {
      this.#outgoingRelease = undefined;
      if (this.#outgoingInput !== outgoing) return;
      this.#outgoingInput = undefined;
      this.#outgoingStageKey = undefined;
      void outgoing.cancel();
    }, this.#fadeDurationMs + OVERFLOW_RELEASE_MARGIN_MS);
    this.#outgoingRelease.unref?.();
  }

  #clearOutgoingRelease(): void {
    if (this.#outgoingRelease) clearTimeout(this.#outgoingRelease);
    this.#outgoingRelease = undefined;
  }

  #applyCrossfadeEvent(event: NativeCrossfadeEvent): void {
    const generation = numericGeneration(event.generation);
    const stage =
      event.entryId === undefined
        ? null
        : {
            entryId: event.entryId,
            queueRevision: numericGeneration(event.queueRevision ?? '0'),
            sourceRevision: numericGeneration(event.sourceRevision ?? '0'),
          };
    const stageKeyValue = stageKey({
      generation:
        event.kind === 'transition_started'
          ? numericGeneration(event.previousGeneration ?? '0')
          : generation,
      entryId: stage?.entryId as string,
      queueRevision: stage?.queueRevision as number,
      sourceRevision: stage?.sourceRevision as number,
    });
    switch (event.kind) {
      case 'timeline_changed': {
        const previous = numericGeneration(event.previousGeneration ?? '0');
        this.#metadata.set(generation, this.#metadata.get(previous));
        this.#boundMetadata(generation);
        rememberGeneration(this.#startedGenerations, generation);
        this.#latestGeneration = generation;
        break;
      }
      case 'prepare_requested': {
        const remainingMs = event.remainingMs ?? 0;
        this.emit(PlaybackEvent.prepareRequested, {
          ...this.#eventContext(generation),
          remainingMs,
          transitionInMs: Math.max(0, remainingMs - this.#fadeDurationMs),
        });
        break;
      }
      case 'stage_ready':
        if (stage)
          this.emit(PlaybackEvent.stageReady, {
            ...this.#eventContext(generation),
            ...stage,
          });
        break;
      case 'stage_failed':
        void this.#stages.get(stageKeyValue)?.input?.cancel();
        this.#stages.delete(stageKeyValue);
        if (stage)
          this.emit(PlaybackEvent.stageFailed, {
            ...this.#eventContext(generation),
            ...stage,
            reason: event.reason ?? 'staged source failed',
          });
        break;
      case 'stage_invalidated':
        void this.#stages.get(stageKeyValue)?.input?.cancel();
        this.#stages.delete(stageKeyValue);
        if (stage) {
          this.emit(PlaybackEvent.stageInvalidated, {
            ...this.#eventContext(generation),
            ...stage,
          });
          if (this.#stageEntryId === stageKeyValue) {
            void this.#stageInput?.cancel();
            this.#stageInput = undefined;
            this.#stageEntryId = undefined;
          }
        }
        break;
      case 'transition_started': {
        if (!stage || !event.info) break;
        const previous = numericGeneration(event.previousGeneration ?? '0');
        // A reconciliation may already have adopted this promotion; its bookkeeping is done.
        if (this.#activeStageKey !== stageKeyValue) {
          const prepared = this.#stages.get(stageKeyValue);
          this.#stages.delete(stageKeyValue);
          this.#metadata.set(generation, prepared?.metadata);
          this.#clearOutgoingRelease();
          this.#outgoingInput = this.#input;
          this.#input = prepared?.input;
          this.#outgoingStageKey = this.#activeStageKey;
          this.#activeStageKey = stageKeyValue;
          if (this.#stageEntryId === stageKeyValue) {
            this.#stageInput = undefined;
            this.#stageEntryId = undefined;
          }
        }
        this.#boundMetadata(generation);
        rememberGeneration(this.#startedGenerations, generation);
        this.#latestGeneration = generation;
        this.emit(PlaybackEvent.playbackStarted, {
          ...this.#eventContext(generation),
          state: 'playing',
          info: toStreamInfo(event.info),
          transition: { type: 'crossfade', outgoingGeneration: previous, entryId: stage.entryId },
        });
        break;
      }
      case 'transition_completed': {
        if (!stage) break;
        const promoted = numericGeneration(event.previousGeneration ?? '0');
        this.#emitTerminal('completed', generation, undefined, undefined, {
          type: 'promoted',
          generation: promoted,
          entryId: stage.entryId,
        });
        this.#clearOutgoingRelease();
        void this.#outgoingInput?.cancel();
        this.#outgoingInput = undefined;
        this.#outgoingStageKey = undefined;
        // The first track is over, which is where its abort binding used to end.
        this.#clearAbortBinding(this.#playController);
        break;
      }
      case 'transition_cancelled': {
        if (!stage) break;
        const failedGeneration = numericGeneration(event.previousGeneration ?? '0');
        // A reconciliation may already have restored the outgoing source.
        if (this.#activeStageKey === stageKeyValue) {
          const abandoned = this.#input;
          this.#input = this.#outgoingInput;
          this.#outgoingInput = undefined;
          this.#activeStageKey = this.#outgoingStageKey;
          this.#outgoingStageKey = undefined;
          this.#clearOutgoingRelease();
          void abandoned?.cancel();
        }
        this.#latestGeneration = generation;
        this.emit(PlaybackEvent.transitionCancelled, {
          ...this.#eventContext(generation),
          failedGeneration,
          entryId: stage.entryId,
        });
        break;
      }
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
    rememberGeneration(this.#terminalGenerations, generation);
    // The terminal set now deduplicates this generation, so its started record can be released.
    this.#startedGenerations.delete(generation);
    const base = this.#eventContext(generation);
    if (state === 'completed') {
      this.emit(PlaybackEvent.playbackCompleted, {
        ...base,
        state,
        position: this.position,
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
