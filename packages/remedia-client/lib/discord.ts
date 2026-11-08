import { randomUUID } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { RemediaEventEmitter } from './events.js';
import {
  PlaybackSessionEvents as PlaybackEvent,
  VoiceSessionEvents as VoiceEvent,
} from './types.js';
import { RemediaRpcError, SessionLostError, asError } from './errors.js';
import { FilterControls, validateVolume } from './filters.js';
import { PlaybackHooks } from './hooks.js';
import type { GuildBackend } from './adapters.js';
import type {
  AudioStreamInfo,
  DiscordSessionEvents,
  DiscordConnectOptions,
  DiscordGatewayPacket,
  DiscordSessionOptions,
  MediaSource,
  PlaybackPosition,
  PlaybackSnapshot,
  PlaybackState,
  PlaybackLifecycleEventMap,
  PlayOptions,
  StageNextOptions,
  StageSnapshot,
  VoiceConnectionState,
  VoiceLifecycleEventMap,
} from './types.js';

interface ConnectAttempt {
  generation: number;
  sequenceFloor: number | undefined;
  resolve(): void;
  reject(error: Error): void;
  promise: Promise<void>;
  timer: ReturnType<typeof setTimeout>;
  signal: AbortSignal | undefined;
  abortListener: (() => void) | undefined;
  requestSent: boolean;
  settled: boolean;
  starting: boolean;
  voiceState: { sessionId: string } | undefined;
  voiceServer: { token: string; endpoint: string } | undefined;
  credentialRevision: number;
  transportGeneration: number | undefined;
  voiceSuspension: Promise<void> | undefined;
  internal: boolean;
  moving?: boolean;
  retryCount: number;
}

/** Internal client services used by a guild session for Gateway routing, backend allocation, and cleanup. */
export interface DiscordSessionRuntime {
  /**
   * Send a voice state request through the caller-owned Discord Gateway connection.
   * @param guildId - Discord guild snowflake used to route the operation.
   * @param channelId - Destination Discord voice channel snowflake; null requests a leave where supported.
   * @param selfDeaf - Discord self-deaf flag; defaults to true.
   * @param selfMute - Discord self-mute flag; defaults to false.
   */
  sendVoicePayload(
    guildId: string,
    channelId: string | null,
    selfDeaf?: boolean,
    selfMute?: boolean,
  ): void | Promise<void>;
  /**
   * Read the last accepted Gateway dispatch sequence.
   * @returns The dispatch sequence, or undefined before one is observed.
   */
  gatewaySequence(): number | undefined;
  /**
   * Allocate or reuse a sticky guild backend under the client assignment lock.
   * @param options - Guild identity, node preference, and retained session configuration.
   * @param onDispose - Callback invoked when this wrapper releases its owning registration.
   * @returns The assigned backend; rejects when the node is unavailable or at capacity.
   */
  resolveBackend(options: DiscordSessionOptions, onDispose: () => void): Promise<GuildBackend>;
  /**
   * Unregister the guild session and release its sticky node assignment.
   * @param session - Guild session whose registration and assignment should be released.
   */
  releaseSession(session: {
    /** Discord guild snowflake as a decimal string; unique within the owning client. */
    readonly guildId: string;
  }): void;
  /** Reserve a destination backend while keeping the original sticky assignment. */
  prepareMigration(options: DiscordSessionOptions, targetNodeId: string): Promise<GuildBackend>;
  /** Switch the sticky assignment only after the original backend is released. */
  commitMigration(guildId: string, fromNodeId: string, toNodeId: string): Promise<void>;
}

// Cover the dedicated 80-second remote voice RPC deadline plus Gateway dispatch latency.
const DEFAULT_CONNECT_TIMEOUT_MS = 100_000;

interface BufferedPlaybackEvent {
  event: keyof PlaybackLifecycleEventMap<unknown>;
  payload: Record<string, unknown>;
}

interface PlaybackAbortBinding {
  controller: AbortController;
  signal: AbortSignal;
  listener: () => void;
  generation: number | undefined;
}

/**
 * A Discord voice player with a node-sticky backend, whether embedded or remote.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export class DiscordSession<Metadata = unknown> extends RemediaEventEmitter<
  DiscordSessionEvents<Metadata>
> {
  /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
  readonly sessionId = randomUUID();
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  readonly guildId: string;
  #channelId: string;
  #selfDeaf: boolean;
  #selfMute: boolean;
  #closing = false;
  #disconnectWork: Promise<void> | undefined;
  #disposeWork: Promise<void> | undefined;
  /** Discord bot user snowflake as a decimal string; separate from Remedia session identity. */
  readonly userId: string;
  readonly #runtime: DiscordSessionRuntime;
  readonly #options: DiscordSessionOptions;
  /** Persistent raw FFmpeg filtergraph controls for this session. */
  readonly filters: FilterControls;
  /** Ordered, awaited local pre-play hooks; listeners are separate from hooks. */
  readonly hooks = new PlaybackHooks<Metadata>();
  readonly #metadataByGeneration = new Map<number, Metadata | undefined>();
  readonly #stageMetadata = new Map<string, Metadata | undefined>();
  readonly #startedGenerations = new Set<number>();
  readonly #terminalGenerations = new Set<number>();
  readonly #earlyPlaybackEvents = new Map<number, BufferedPlaybackEvent[]>();
  #pendingPlay: { metadata: Metadata | undefined } | undefined;
  #playStarting = false;
  #playController: AbortController | undefined;
  #playAbortBinding: PlaybackAbortBinding | undefined;
  #boundBackend: GuildBackend | undefined;
  readonly #backendUnsubscribers: Array<() => void> = [];
  #backend: GuildBackend | undefined;
  #backendWork: Promise<GuildBackend> | undefined;
  #attempt: ConnectAttempt | undefined;
  #joinRequested = false;
  #disposed = false;
  #connectionState: VoiceConnectionState = 'disconnected';
  #connectionError: string | null = null;
  #filterGraph: string | null = null;
  #volume = 1;
  #voiceSessionId: string | undefined;
  #voiceServer: { token: string; endpoint: string } | undefined;
  #gatewayEpoch: number | undefined;
  #lastGatewaySequence: number | undefined;
  #configurationWork: Promise<void> = Promise.resolve();
  #connectionGeneration = 0;
  #transportGeneration = 0;
  #playbackGeneration = 0;
  #playPromise: Promise<AudioStreamInfo> | undefined;
  #sessionLossReason: string | undefined;
  #voiceFailureEmitted = false;
  #migrationWork: Promise<void> | undefined;
  #migratableSource: { url: string } | undefined;
  #migratableMetadata: Metadata | undefined;

  /**
   * Construct this wrapper with its owning services. Use the owning engine or client factory for application code.
   * @param runtime - Owning client services for Gateway routing, backend allocation, and cleanup.
   * @param options - Configuration and lifecycle options for this operation.
   */
  constructor(runtime: DiscordSessionRuntime, options: DiscordSessionOptions) {
    super();
    this.#runtime = runtime;
    this.#options = { ...options };
    this.guildId = validateSnowflake('guildId', options.guildId);
    this.#channelId = validateSnowflake('channelId', options.channelId);
    this.userId = validateSnowflake('userId', options.userId);
    validateVolume(options.volume ?? 1);
    if (
      options.filterGraph !== undefined &&
      options.filterGraph !== null &&
      (typeof options.filterGraph !== 'string' || !options.filterGraph.trim())
    )
      throw new TypeError('filterGraph must be a non-empty string or null');
    for (const value of [options.selfDeaf, options.selfMute]) {
      if (value !== undefined && typeof value !== 'boolean')
        throw new TypeError('voice flags must be boolean');
    }
    this.#volume = options.volume ?? 1;
    this.#filterGraph = options.filterGraph ?? null;
    this.#selfDeaf = options.selfDeaf ?? true;
    this.#selfMute = options.selfMute ?? false;
    this.filters = new FilterControls(
      () => this.#filterGraph,
      (graph) => this.#replaceFilterGraph(graph),
      () => this.#replaceFilterGraph(null),
    );
  }

  /** Current requested voice destination; independent of playback identity. */
  get channelId(): string {
    return this.#channelId;
  }
  /** Discord Gateway self-deaf state; defaults to true and persists across moves and recovery. */
  get selfDeaf(): boolean {
    return this.#selfDeaf;
  }
  /** Discord Gateway self-mute state; defaults to false and persists across moves and recovery. */
  get selfMute(): boolean {
    return this.#selfMute;
  }

  /**
   * Move this Discord session to another configured node. The public session object remains
   * stable. Active URL playback is restarted on the destination and sought to the last observed
   * media position; an exact or gapless audio handoff is not guaranteed. One-shot sources and
   * active local files are rejected before the source backend is changed.
   *
   * The destination is reserved before stopping the original backend. If connection or replay
   * fails after cutover, the destination remains assigned and connect() may be retried.
   */
  migrate(nodeId: string): Promise<void> {
    this.#assertOpen();
    if (this.#migrationWork) return this.#migrationWork;
    const work = this.#migrate(nodeId);
    this.#migrationWork = work;
    void work
      .finally(() => {
        if (this.#migrationWork === work) this.#migrationWork = undefined;
      })
      .catch(() => undefined);
    return work;
  }

  async #migrate(nodeId: string): Promise<void> {
    if (!nodeId || typeof nodeId !== 'string')
      throw new TypeError('nodeId must be a non-empty string');
    const previous = this.#backend;
    if (!previous) throw new Error('connect the Discord session before migrating');
    if (previous.nodeId === nodeId) return;
    if (
      this.#attempt ||
      this.#playStarting ||
      this.#disconnectWork ||
      this.#closing ||
      this.connectionState !== 'connected'
    )
      throw new Error('cannot migrate during connection, playback startup, or disconnection');

    await this.#configurationWork;
    const snapshot = await previous.refresh();
    const active = snapshot.state === 'playing' || snapshot.state === 'paused';
    if (!active && isPlaybackActive(snapshot.state))
      throw new Error('cannot migrate while playback is transitioning between states');
    const source = active ? this.#migratableSource : undefined;
    const metadata = active ? this.#migratableMetadata : undefined;
    if (
      active &&
      (!source || snapshot.position.mediaMs === null || !Number.isFinite(snapshot.position.mediaMs))
    )
      throw new Error('active migration requires a replayable URL and a known media position');
    const positionMs = active ? Math.max(0, snapshot.position.mediaMs!) : 0;
    const paused = snapshot.state === 'paused' || snapshot.state === 'pausing';
    const destination = await this.#runtime.prepareMigration(
      {
        ...this.#options,
        channelId: this.channelId,
        volume: this.#volume,
        filterGraph: this.#filterGraph,
        selfDeaf: this.#selfDeaf,
        selfMute: this.#selfMute,
      },
      nodeId,
    );
    let cutover = false;
    try {
      this.#assertOpen();
      this.#detachBackend();
      this.#backend = undefined;
      await previous.disconnect(this.#nextTransportGeneration());
      await previous.dispose();
      cutover = true;
      await this.#runtime.commitMigration(this.guildId, previous.nodeId, destination.nodeId);
      this.#options.nodeId = destination.nodeId;
      this.#backend = destination;
      this.#bindBackend(destination);
      this.#clearPlayAbortBinding();
      this.#startedGenerations.clear();
      this.#terminalGenerations.clear();
      this.#metadataByGeneration.clear();
      this.#earlyPlaybackEvents.clear();
      this.#playbackGeneration = 0;
      this.#playPromise = undefined;
      this.#migratableSource = undefined;
      this.#migratableMetadata = undefined;
      this.#joinRequested = false;
      this.#voiceSessionId = undefined;
      this.#voiceServer = undefined;
      this.#connectionError = null;
      this.#voiceFailureEmitted = false;
      this.#setConnectionState('disconnected');
      await this.connect();
      if (source) {
        // The replacement decoder uses its own source generation, so replay is a new start.
        this.#playPromise = this.#startPlayback(
          source,
          metadata === undefined ? {} : { customMetadata: metadata },
        );
        await this.#playPromise;
        if (positionMs > 0) await destination.seek(positionMs);
        if (paused) await destination.pause();
      }
    } catch (error) {
      if (!cutover) {
        await destination.dispose().catch(() => undefined);
        if (this.#backend === undefined) {
          // An unsuccessful old-backend teardown may leave the old node recoverable.
          this.#backend = previous;
          this.#bindBackend(previous);
        }
      } else if (this.#backend === undefined) {
        this.#backend = destination;
        this.#bindBackend(destination);
        this.#setConnectionState('failed');
      }
      throw error;
    }
  }

  /**
   * Replace only the voice transport. A newer move supersedes a pending move.
   * @param channelId - Destination Discord voice channel snowflake as a positive decimal string.
   * @returns Resolves when the requested destination connects; rejects on supersession, cancellation, or connection failure.
   */
  move(channelId: string): Promise<void> {
    this.#assertOpen();
    validateSnowflake('channelId', channelId);
    if (this.#connectionState === 'disconnecting')
      return Promise.reject(new Error('Discord session is disconnecting'));
    if (channelId === this.channelId) return this.connect();
    const previous = this.#attempt;
    if (previous) {
      this.#finishAttempt(previous);
      previous.reject(abortError('Discord voice connection was superseded by move()'));
    }
    this.#channelId = channelId;
    this.#options.channelId = channelId;
    if (!this.#backend) return this.connect();
    const attempt = this.#newAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true);
    attempt.moving = true;
    this.#attempt = attempt;
    this.#voiceSessionId = undefined;
    this.#voiceServer = undefined;
    this.#setConnectionState('connecting');
    this.#suspendRemoteVoice(attempt);
    void this.#connect(attempt, {});
    return attempt.promise;
  }

  /**
   * Update Discord self-deaf state without rebuilding playback. Unchanged values send nothing.
   * @param enabled - The requested Discord self-state flag.
   * @returns Resolves after storing the flag or sending its Gateway update; rejects if the update fails.
   */
  setSelfDeaf(enabled: boolean): Promise<void> {
    return this.#setVoiceFlag('deaf', enabled);
  }
  /**
   * Update Discord self-mute state without changing audio gain or playback pause. Unchanged values send nothing.
   * @param enabled - The requested Discord self-state flag.
   * @returns Resolves after storing the flag or sending its Gateway update; rejects if the update fails.
   */
  setSelfMute(enabled: boolean): Promise<void> {
    return this.#setVoiceFlag('mute', enabled);
  }

  async #setVoiceFlag(flag: 'deaf' | 'mute', enabled: boolean): Promise<void> {
    this.#assertOpen();
    if (typeof enabled !== 'boolean') throw new TypeError('voice flag must be boolean');
    await this.#serializeConfiguration(async () => {
      this.#assertOpen();
      if (this.#connectionState === 'disconnecting')
        throw new Error('Discord session is disconnecting');
      if ((flag === 'deaf' ? this.#selfDeaf : this.#selfMute) === enabled) return;
      if (this.#backend?.nodeType === 'embedded') {
        if (flag === 'deaf') await this.#backend.setSelfDeaf?.(enabled);
        else await this.#backend.setSelfMute?.(enabled);
      } else if (this.#joinRequested) {
        await this.#runtime.sendVoicePayload(
          this.guildId,
          this.channelId,
          flag === 'deaf' ? enabled : this.#selfDeaf,
          flag === 'mute' ? enabled : this.#selfMute,
        );
      }
      this.#options.selfDeaf = flag === 'deaf' ? enabled : this.#selfDeaf;
      this.#options.selfMute = flag === 'mute' ? enabled : this.#selfMute;
      if (flag === 'deaf') this.#selfDeaf = enabled;
      else this.#selfMute = enabled;
    });
  }

  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  get nodeId(): string | null {
    return this.#backend?.nodeId ?? null;
  }

  /** Execution mode of the assigned backend, or null before assignment. */
  get nodeType(): 'embedded' | 'remote' | null {
    return this.#backend?.nodeType ?? null;
  }

  /** Current Discord transport lifecycle state, independent of playback pause or completion. */
  get connectionState(): VoiceConnectionState {
    if (this.#disposed) return 'disconnected';
    if (
      this.#connectionState === 'disconnected' ||
      this.#connectionState === 'connecting' ||
      this.#connectionState === 'negotiating_dave' ||
      this.#connectionState === 'disconnecting' ||
      this.#connectionState === 'failed'
    )
      return this.#connectionState;
    return this.#backend?.connectionState ?? this.#connectionState;
  }

  /** Most recent voice connection failure message, or null when no error is available. */
  get connectionError(): string | null {
    return this.#connectionError ?? this.#backend?.error ?? null;
  }

  /** Current playback lifecycle state. */
  get state(): PlaybackState {
    return this.#backend?.state ?? (this.connectionState === 'failed' ? 'failed' : 'idle');
  }

  /** Latest source and output timeline measurements in milliseconds. */
  get position(): PlaybackPosition {
    return this.#backend?.position ?? emptyPosition();
  }

  /** Source duration in milliseconds, or null when unknown. */
  get durationMs(): number | null {
    return this.#backend?.durationMs ?? null;
  }

  /** Most recent playback or voice failure message, or null when no error is available. */
  get error(): string | null {
    return this.#backend?.error ?? this.#connectionError;
  }

  /** Session master linear gain from 0 through 2, independent of filtergraph processing; defaults to 1. */
  get volume(): number {
    return this.#backend?.volume ?? this.#volume;
  }

  /** Latest known playback snapshot, including persistent configuration and source generation. */
  get snapshot(): PlaybackSnapshot {
    return this.#backend?.snapshot ?? emptySnapshot(this.#filterGraph, this.#volume);
  }

  /** Whether a backend is unassigned, assigned, or lost after its remote session disappears. */
  get assignmentStatus(): 'unassigned' | 'assigned' | 'lost' {
    if (!this.#backend) return 'unassigned';
    return this.#backend.state === 'failed' && this.#backend.error?.includes('session')
      ? 'lost'
      : 'assigned';
  }

  /**
   * Join the requested voice destination and wait for Gateway credentials and transport readiness. Concurrent calls share the pending attempt.
   * @param options - Optional cancellation signal and positive timeout in milliseconds; defaults to 100000.
   * @returns Resolves when connected; rejects on timeout, cancellation, session loss, or transport failure.
   */
  connect(options: DiscordConnectOptions = {}): Promise<void> {
    this.#assertOpen();
    if (this.#attempt) return this.#attempt.promise;
    if (this.connectionState === 'connected' || this.connectionState === 'reconnecting')
      return Promise.resolve();
    if (this.connectionState === 'disconnecting')
      return Promise.reject(new Error('Discord session is disconnecting'));
    if (options.signal?.aborted)
      return Promise.reject(abortError('Discord voice connection was cancelled'));
    const timeoutMs = options.timeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS;
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0)
      return Promise.reject(new RangeError('timeoutMs must be positive'));

    const attempt = this.#newAttempt(timeoutMs, options.signal, false);
    this.#attempt = attempt;
    this.#voiceSessionId = undefined;
    this.#voiceServer = undefined;
    this.#voiceFailureEmitted = false;
    this.#setConnectionState('connecting');
    this.#connectionError = null;

    void this.#connect(attempt, options);
    return attempt.promise;
  }

  /**
   * Open one media source and begin a new playback generation using the persistent session configuration. Stop active playback before starting another source.
   * @param source - Encoded media input; Buffer and caller-owned binary Readable inputs require embedded mode.
   * @param options - Optional metadata for this generation and a cancellation signal.
   * @returns Selected stream metadata once startup succeeds; rejects if hooks, input opening, or playback startup fail.
   */
  play(source: MediaSource, options: PlayOptions<Metadata> = {}): Promise<AudioStreamInfo> {
    if (this.#migrationWork)
      return Promise.reject(new Error('playback cannot start during node migration'));
    const work = this.#startPlayback(source, options);
    this.#playPromise = work;
    return work;
  }

  async #startPlayback(
    source: MediaSource,
    options: PlayOptions<Metadata>,
  ): Promise<AudioStreamInfo> {
    this.#assertOpen();
    if (this.connectionState !== 'connected' || !this.#backend) {
      throw new Error('Discord voice session is not connected');
    }
    if (this.#playStarting || isPlaybackActive(this.state)) {
      throw new Error('playback is already active; stop it before starting another source');
    }
    if (options.signal?.aborted) throw abortError('playback was cancelled');

    this.#playStarting = true;
    const backend = this.#backend;
    const controller = new AbortController();
    this.#playController = controller;
    let backendStarted = false;
    let keepAbortBinding = false;
    let abortBinding: PlaybackAbortBinding | undefined;
    if (options.signal) {
      abortBinding = {
        controller,
        signal: options.signal,
        generation: undefined,
        listener: () => {
          if (
            abortBinding?.generation !== undefined &&
            abortBinding.generation !== this.#playbackGeneration
          )
            return;
          controller.abort(options.signal?.reason);
          if (backendStarted) void backend.stop().catch(() => undefined);
        },
      };
      this.#playAbortBinding = abortBinding;
      options.signal.addEventListener('abort', abortBinding.listener, { once: true });
      // Close the check/listener-registration race. EventTarget does not replay an abort event
      // for listeners added after the signal was aborted.
      if (options.signal.aborted) abortBinding.listener();
    }
    this.#pendingPlay = { metadata: options.customMetadata };
    this.#earlyPlaybackEvents.clear();
    try {
      await this.hooks.runBeforePlay(source, options.customMetadata, controller.signal);
      this.#assertOpen();
      if (controller.signal.aborted) throw abortError('playback was cancelled');
      await this.#configurationWork;
      this.#assertOpen();
      if (controller.signal.aborted) throw abortError('playback was cancelled');
      const playRequest = backend.play(source, {
        ...(options.customMetadata === undefined ? {} : { customMetadata: options.customMetadata }),
        signal: controller.signal,
      });
      backendStarted = true;
      const result = await playRequest;
      this.#playbackGeneration = result.generation;
      if (abortBinding) abortBinding.generation = result.generation;
      this.#metadataByGeneration.set(result.generation, options.customMetadata);
      this.#migratableSource = 'url' in source ? { url: source.url } : undefined;
      this.#migratableMetadata = options.customMetadata;
      this.#boundMetadata(result.generation);
      this.#flushEarlyEvents(result.generation);
      if (controller.signal.aborted) throw abortError('playback was cancelled');
      keepAbortBinding = abortBinding !== undefined && !isPlaybackTerminal(this.state);
      return result.info;
    } catch (error) {
      this.#flushPendingFailure();
      if (controller.signal.aborted) throw abortError('playback was cancelled');
      throw error;
    } finally {
      this.#pendingPlay = undefined;
      this.#playStarting = false;
      if (!keepAbortBinding) this.#clearPlayAbortBinding(abortBinding);
      if (this.#playController === controller) {
        this.#playController = undefined;
      }
    }
  }

  /**
   * Pause the current playback without replacing its decoder or source generation.
   * @returns Resolves when the backend accepts the pause request.
   */
  /** Prepare the next queue entry without interrupting the active source. */
  async stageNext(source: MediaSource, options: StageNextOptions<Metadata>): Promise<StageSnapshot> {
    this.#assertOpen();
    const backend = this.#requireExistingBackend();
    const result = await backend.stageNext(source, options);
    this.#stageMetadata.set(result.entryId, options.customMetadata);
    return result;
  }

  clearStage(): Promise<void> {
    this.#stageMetadata.clear();
    return this.#requireExistingBackend().clearStage();
  }

  stagedSource(): Promise<StageSnapshot | null> {
    return this.#requireExistingBackend().stagedSource();
  }

  pause(): Promise<void> {
    return this.#requireExistingBackend().pause();
  }
  /**
   * Resume paused playback on its existing source generation.
   * @returns Resolves when the backend accepts the resume request.
   */
  resume(): Promise<void> {
    return this.#requireExistingBackend().resume();
  }
  /**
   * Seek on the original media timeline; non-seekable and one-shot sources may reject.
   * @param positionMs - Non-negative finite position on the original media timeline, in milliseconds.
   * @returns Resolves after the backend accepts the seek; rejects for invalid positions or unsupported inputs.
   */
  seek(positionMs: number): Promise<void> {
    if (!Number.isFinite(positionMs) || positionMs < 0)
      return Promise.reject(new RangeError('positionMs must be non-negative'));
    return this.#requireExistingBackend().seek(positionMs);
  }
  /**
   * Stop the active playback generation and release its input without leaving the voice channel.
   * @returns Resolves after the backend stops the active source.
   */
  stop(): Promise<void> {
    return this.#stopPlayback();
  }

  async #stopPlayback(): Promise<void> {
    await this.#requireExistingBackend().stop();
    this.#clearPlayAbortBinding();
  }

  /**
   * Wait for the current playback generation to reach a terminal state.
   * @returns The terminal playback state; a superseded client generation is reported as stopped.
   */
  async wait(): Promise<PlaybackState> {
    this.#assertOpen();
    if (this.#playPromise) await this.#playPromise;
    const completion = await this.#requireExistingBackend().wait();
    if (completion.generation !== this.#playbackGeneration) return 'stopped';
    return completion.state;
  }

  /**
   * Refresh the latest playback snapshot from its backend.
   * @returns The refreshed snapshot, or the retained initial snapshot before backend allocation.
   */
  async refresh(): Promise<PlaybackSnapshot> {
    this.#assertOpen();
    if (!this.#backend) return this.snapshot;
    return this.#backend.refresh();
  }

  /**
   * Set persistent linear master gain independently of the configured filtergraph; unchanged gain emits no change event.
   * @param gain - Finite linear master gain from 0 through 2; 1 leaves amplitude unchanged.
   * @returns Resolves after the gain is stored or applied; rejects for non-finite gain or values outside 0 through 2.
   * @throws {RangeError} If gain is non-finite or outside the inclusive range 0 through 2.
   */
  async setVolume(gain: number): Promise<void> {
    this.#assertOpen();
    validateVolume(gain);
    await this.#serializeConfiguration(async () => {
      if (this.#connectionState === 'disconnecting')
        throw new Error('Discord session is disconnecting');
      const previousVolume = this.volume;
      if (previousVolume === gain) return;
      if (this.#backend) {
        await this.#backend.setVolume(gain);
      } else {
        this.#publishPlaybackEvent(PlaybackEvent.volumeChanged, {
          sessionId: this.sessionId,
          guildId: this.guildId,
          nodeId: this.nodeId,
          generation: this.#playbackGeneration,
          previousVolume,
          volume: gain,
        });
      }
      this.#volume = gain;
    });
  }

  /**
   * Cancel connection attempts, leave Gateway voice, and close the transport and active playback. The wrapper can reconnect afterward.
   * @returns Resolves after cleanup; concurrent disconnects share cleanup work.
   */
  disconnect(): Promise<void> {
    if (this.#disposed) return Promise.resolve();
    if (this.#disconnectWork) return this.#disconnectWork;
    const work = this.#disconnect(true);
    this.#disconnectWork = work.finally(() => {
      this.#disconnectWork = undefined;
    });
    return this.#disconnectWork;
  }

  /**
   * Disconnect and unregister this session, releasing its owned resources. Repeated calls share disposal work.
   * @returns Resolves after disposal; rejects if resource cleanup fails.
   */
  dispose(): Promise<void> {
    if (this.#disposeWork) return this.#disposeWork;
    this.#disposeWork = this.#dispose();
    return this.#disposeWork;
  }

  async #dispose(): Promise<void> {
    if (this.#disposed) return;
    this.#closing = true;
    await this.#migrationWork?.catch(() => undefined);
    const abortBinding = this.#playAbortBinding;
    this.#clearPlayAbortBinding(abortBinding);
    (abortBinding?.controller ?? this.#playController)?.abort(
      new Error('Discord session disposed'),
    );
    this.#closing = true;
    try {
      await this.disconnect();
    } finally {
      this.#disposed = true;
      try {
        this.#detachBackend();
        await this.#backend?.dispose();
      } finally {
        this.hooks.clear();
        this.#metadataByGeneration.clear();
        this.#earlyPlaybackEvents.clear();
        this.#migratableSource = undefined;
        this.#migratableMetadata = undefined;
        this.emit(PlaybackEvent.sessionDisposed, {
          sessionId: this.sessionId,
          guildId: this.guildId,
          nodeId: this.nodeId,
          generation: this.#playbackGeneration,
        });
        this.#backend = undefined;
        this.#runtime.releaseSession(this);
      }
    }
  }

  /**
   * Route a validated guild voice dispatch through the current connection attempt. Stale sequences and unrelated users or channels are ignored.
   *
   * @internal
   * @param event - Event name identifying the typed notification or Gateway voice dispatch.
   * @param data - Validated guild-scoped Gateway dispatch data.
   * @param sequence - Gateway dispatch sequence, when supplied by the host application.
   * @param epoch - Gateway READY epoch used to reset stale-sequence tracking.
   */
  receiveGatewayDispatch(
    event: string,
    data: Record<string, unknown>,
    sequence?: number,
    epoch?: number,
  ): void {
    if (this.#disposed || this.#backend?.nodeType !== 'remote' || data.guild_id !== this.guildId)
      return;
    if (epoch !== undefined && epoch !== this.#gatewayEpoch) {
      this.#gatewayEpoch = epoch;
      this.#lastGatewaySequence = undefined;
      if (this.#attempt) this.#attempt.sequenceFloor = undefined;
    }
    if (
      sequence !== undefined &&
      this.#lastGatewaySequence !== undefined &&
      sequence <= this.#lastGatewaySequence
    ) {
      return;
    }
    if (sequence !== undefined) this.#lastGatewaySequence = sequence;
    const attempt = this.#attempt;
    if (
      attempt &&
      sequence !== undefined &&
      attempt.sequenceFloor !== undefined &&
      sequence <= attempt.sequenceFloor
    )
      return;

    if (event === 'VOICE_STATE_UPDATE') this.#receiveVoiceState(data);
    else if (event === 'VOICE_SERVER_UPDATE') this.#receiveVoiceServer(data);
  }

  /**
   * Mark the remote session unavailable and fail pending operations for its retained assignment.
   *
   * @internal
   * @param reason - Explanation for remote session loss or operation failure.
   */
  markSessionLost(reason: string): void {
    this.#sessionLossReason = reason;
    const active = isPlaybackActive(this.state);
    const abortBinding = this.#playAbortBinding;
    this.#clearPlayAbortBinding(abortBinding);
    (abortBinding?.controller ?? this.#playController)?.abort(new Error(reason));
    this.#backend?.markSessionLost(reason);
    this.#connectionError = reason;
    this.#setConnectionState('failed');
    this.#emitVoiceFailure(new SessionLostError(this.nodeId ?? 'unknown', [this.guildId], reason));
    if (active) {
      this.#publishPlaybackEvent(PlaybackEvent.playbackFailed, {
        sessionId: this.sessionId,
        guildId: this.guildId,
        nodeId: this.nodeId,
        generation: this.#playbackGeneration,
        state: 'failed',
        error: new SessionLostError(this.nodeId ?? 'unknown', [this.guildId], reason),
      });
    }
    const attempt = this.#attempt;
    if (attempt)
      this.#failAttempt(attempt, new Error(`Remote Remedia session was lost: ${reason}`), false);
  }

  async #connect(attempt: ConnectAttempt, options: DiscordConnectOptions): Promise<void> {
    try {
      const backend = await this.#ensureBackend();
      if (!this.#isCurrent(attempt)) return;
      await this.#serializeConfiguration(() => this.#applyConfiguration(backend));
      if (!this.#isCurrent(attempt)) return;
      if (backend.nodeType === 'embedded') {
        if (attempt.moving) await backend.move?.(this.channelId);
        else await backend.connect(options);
        if (!this.#isCurrent(attempt)) {
          return;
        }
        this.#setConnectionState('connected');
        this.#finishAttempt(attempt);
        attempt.resolve();
      } else {
        backend.validateIdentity?.(this.userId);
        this.#setConnectionState(
          attempt.internal && !attempt.moving ? 'reconnecting' : 'connecting',
        );
        this.#joinRequested = true;
        await this.#runtime.sendVoicePayload(
          this.guildId,
          this.channelId,
          this.#selfDeaf,
          this.#selfMute,
        );
        if (!this.#isCurrent(attempt)) return;
        attempt.requestSent = true;
        this.#tryStartRemoteConnection(attempt);
      }
    } catch (error) {
      this.#failAttempt(attempt, asError(error), this.#joinRequested);
    }
  }

  #receiveVoiceState(data: Record<string, unknown>): void {
    if (data.user_id !== this.userId || !this.#joinRequested) return;
    if (data.channel_id === null) {
      if (this.#attempt?.moving) return;
      if (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting') {
        this.#handleExternalDisconnect();
      }
      return;
    }
    if (
      data.channel_id !== this.channelId ||
      typeof data.session_id !== 'string' ||
      data.session_id.length === 0
    )
      return;
    if (data.session_id === this.#voiceSessionId) return;
    const refresh =
      !this.#attempt &&
      ['connected', 'reconnecting', 'disconnected', 'failed'].includes(this.connectionState);
    const attempt =
      this.#attempt ??
      (refresh ? this.#newAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true) : undefined);
    if (!attempt) return;
    if (refresh) {
      this.#attempt = attempt;
      attempt.voiceServer = undefined;
      attempt.voiceState = { sessionId: data.session_id };
      attempt.credentialRevision += 1;
      this.#voiceSessionId = data.session_id;
      this.#setConnectionState('reconnecting');
      this.#suspendRemoteVoice(attempt);
      void Promise.resolve(
        this.#runtime.sendVoicePayload(
          this.guildId,
          this.channelId,
          this.#selfDeaf,
          this.#selfMute,
        ),
      )
        .then(() => {
          if (this.#attempt === attempt) attempt.requestSent = true;
          this.#tryStartRemoteConnection(attempt);
        })
        .catch((error: unknown) => {
          if (!this.#retryGatewayCredentials(attempt, asError(error))) {
            this.#failAttempt(attempt, asError(error), true);
          }
        });
      return;
    }
    attempt.voiceState = { sessionId: data.session_id };
    this.#voiceSessionId = data.session_id;
    attempt.credentialRevision += 1;
    this.#suspendRemoteVoice(attempt);
    this.#tryStartRemoteConnection(attempt);
  }

  #receiveVoiceServer(data: Record<string, unknown>): void {
    if (!this.#joinRequested) return;
    if (data.endpoint === null) {
      const attempt = this.#attempt;
      if (attempt?.internal) {
        this.#retryGatewayCredentials(attempt, new Error('Discord voice endpoint is rotating'));
      } else if (attempt) {
        this.#failAttempt(
          attempt,
          new Error('Discord did not provide a voice server endpoint'),
          true,
        );
      } else if (
        ['connected', 'reconnecting', 'disconnected', 'failed'].includes(this.connectionState)
      ) {
        this.#beginCredentialRefresh(true);
      }
      return;
    }
    if (
      typeof data.token !== 'string' ||
      !data.token ||
      typeof data.endpoint !== 'string' ||
      !data.endpoint
    )
      return;
    const sameServer =
      this.#voiceServer?.token === data.token && this.#voiceServer.endpoint === data.endpoint;
    const currentState = this.connectionState;
    const refresh =
      !this.#attempt &&
      !['connecting', 'negotiating_dave', 'disconnecting'].includes(currentState) &&
      (currentState !== 'connected' || !sameServer);
    if (sameServer && !refresh && this.#attempt?.voiceServer?.token === data.token) return;
    this.#voiceServer = { token: data.token, endpoint: data.endpoint };
    const attempt =
      this.#attempt ??
      (refresh ? this.#newAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true) : undefined);
    if (!attempt) return;
    if (refresh) {
      this.#attempt = attempt;
      attempt.voiceState = this.#voiceSessionId ? { sessionId: this.#voiceSessionId } : undefined;
      attempt.voiceServer = { token: data.token, endpoint: data.endpoint };
      attempt.credentialRevision += 1;
      this.#setConnectionState('reconnecting');
      this.#suspendRemoteVoice(attempt);
      void Promise.resolve(
        this.#runtime.sendVoicePayload(
          this.guildId,
          this.channelId,
          this.#selfDeaf,
          this.#selfMute,
        ),
      )
        .then(() => {
          if (this.#attempt === attempt) attempt.requestSent = true;
          this.#tryStartRemoteConnection(attempt);
        })
        .catch((error: unknown) => {
          if (!this.#retryGatewayCredentials(attempt, asError(error))) {
            this.#failAttempt(attempt, asError(error), true);
          }
        });
      return;
    }
    attempt.voiceServer = { token: data.token, endpoint: data.endpoint };
    attempt.credentialRevision += 1;
    this.#suspendRemoteVoice(attempt);
    this.#tryStartRemoteConnection(attempt);
  }

  #beginCredentialRefresh(requireFreshServer: boolean): void {
    if (this.#attempt || this.#disposed || !this.#joinRequested) return;
    const attempt = this.#newAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true);
    attempt.voiceState = this.#voiceSessionId ? { sessionId: this.#voiceSessionId } : undefined;
    attempt.voiceServer = requireFreshServer ? undefined : this.#voiceServer;
    attempt.credentialRevision += 1;
    this.#attempt = attempt;
    this.#setConnectionState('reconnecting');
    this.#suspendRemoteVoice(attempt);
    void Promise.resolve(
      this.#runtime.sendVoicePayload(this.guildId, this.channelId, this.#selfDeaf, this.#selfMute),
    )
      .then(() => {
        if (!this.#isCurrent(attempt)) return;
        attempt.requestSent = true;
        this.#tryStartRemoteConnection(attempt);
      })
      .catch((error: unknown) => {
        if (!this.#retryGatewayCredentials(attempt, asError(error))) {
          this.#failAttempt(attempt, asError(error), true);
        }
      });
  }

  #tryStartRemoteConnection(attempt: ConnectAttempt): void {
    if (
      this.#attempt !== attempt ||
      attempt.starting ||
      !attempt.requestSent ||
      !attempt.voiceState ||
      !attempt.voiceServer
    )
      return;
    attempt.starting = true;
    void (async () => {
      const backend = await this.#ensureBackend();
      let installed: { sessionId: string; token: string; endpoint: string } | undefined;
      while (this.#isCurrent(attempt)) {
        const state = attempt.voiceState;
        const server = attempt.voiceServer;
        if (!state || !server || !attempt.requestSent) return;
        const transportGeneration =
          attempt.transportGeneration ?? this.#nextTransportGeneration(attempt);
        const credentials = {
          channelId: this.channelId,
          sessionId: state.sessionId,
          token: server.token,
          endpoint: server.endpoint,
        };
        const credentialRevision = attempt.credentialRevision;
        try {
          await attempt.voiceSuspension;
          if (credentialRevision !== attempt.credentialRevision) continue;
          await this.#serializeConfiguration(async () => {
            if (!this.#isCurrent(attempt)) return;
            await backend.connectWithCredentials?.(credentials, transportGeneration);
          });
        } catch (error) {
          if (!this.#isCurrent(attempt)) return;
          if (credentialRevision !== attempt.credentialRevision) continue;
          throw error;
        }
        if (!this.#isCurrent(attempt)) {
          // The backend fences this request with transportGeneration. A stale RPC
          // must not issue a broad disconnect that could stop a newer transport
          // or the playback session it carries.
          return;
        }
        if (credentialRevision !== attempt.credentialRevision) continue;
        installed = credentials;
        if (
          attempt.voiceState?.sessionId === credentials.sessionId &&
          attempt.voiceServer?.token === credentials.token &&
          attempt.voiceServer?.endpoint === credentials.endpoint
        ) {
          break;
        }
      }
      if (!this.#isCurrent(attempt) || !installed) return;
      this.#joinRequested = true;
      this.#voiceSessionId = installed.sessionId;
      this.#voiceServer = { token: installed.token, endpoint: installed.endpoint };
      this.#connectionError = null;
      this.#setConnectionState('connected');
      this.#finishAttempt(attempt);
      attempt.resolve();
    })()
      .catch((error: unknown) => {
        attempt.starting = false;
        if (!this.#retryGatewayCredentials(attempt, asError(error))) {
          this.#failAttempt(attempt, asError(error), true);
        }
      })
      .finally(() => {
        if (this.#attempt === attempt && !attempt.settled) {
          attempt.starting = false;
          this.#tryStartRemoteConnection(attempt);
        }
      });
  }

  #suspendRemoteVoice(attempt: ConnectAttempt): void {
    const transportGeneration = this.#nextTransportGeneration(attempt);
    const backend = this.#backend;
    if (backend?.nodeType !== 'remote' || !backend.suspendVoiceRecovery) return;
    const suspension = backend.suspendVoiceRecovery(transportGeneration);
    attempt.voiceSuspension = suspension;
    void suspension.catch(() => undefined);
  }

  #nextTransportGeneration(attempt?: ConnectAttempt): number {
    if (this.#transportGeneration >= Number.MAX_SAFE_INTEGER - 1) {
      throw new RangeError('Discord voice transport generation has been exhausted');
    }
    this.#transportGeneration += 1;
    if (attempt) attempt.transportGeneration = this.#transportGeneration;
    return this.#transportGeneration;
  }

  #newAttempt(
    timeoutMs: number,
    signal: AbortSignal | undefined,
    internal: boolean,
  ): ConnectAttempt {
    let resolve!: () => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<void>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    // Cancellation during close must remain an observable rejection for callers without creating
    // a process-level unhandled rejection when teardown wins a race with connection startup.
    void promise.catch(() => undefined);
    const attempt = {} as ConnectAttempt;
    const onTimeout = (): void => {
      const error = new Error(`Discord voice connection timed out after ${timeoutMs} ms`);
      if (this.#retryGatewayCredentials(attempt, error)) {
        attempt.timer = setTimeout(onTimeout, timeoutMs);
      } else {
        this.#failAttempt(attempt, error, true);
      }
    };
    Object.assign(attempt, {
      generation: this.#connectionGeneration + 1,
      sequenceFloor: this.#runtime.gatewaySequence(),
      resolve,
      reject,
      promise,
      timer: setTimeout(onTimeout, timeoutMs),
      signal,
      abortListener: undefined,
      requestSent: false,
      settled: false,
      starting: false,
      voiceState: undefined,
      voiceServer: undefined,
      credentialRevision: 0,
      transportGeneration: undefined,
      voiceSuspension: undefined,
      internal,
      retryCount: 0,
    } satisfies ConnectAttempt);
    if (internal) void attempt.promise.catch(() => undefined);
    this.#connectionGeneration += 1;
    if (signal) {
      attempt.abortListener = () =>
        this.#failAttempt(attempt, abortError('Discord voice connection was cancelled'), true);
      signal.addEventListener('abort', attempt.abortListener, { once: true });
    }
    return attempt;
  }

  async #disconnect(sendGatewayLeave: boolean): Promise<void> {
    const attempt = this.#attempt;
    const shouldLeave = this.#joinRequested || attempt !== undefined;
    const abortBinding = this.#playAbortBinding;
    this.#clearPlayAbortBinding(abortBinding);
    (abortBinding?.controller ?? this.#playController)?.abort(
      abortError('Discord voice session disconnected'),
    );
    this.#joinRequested = false;
    this.#setConnectionState('disconnecting');
    const transportGeneration = this.#nextTransportGeneration();
    if (attempt) {
      this.#finishAttempt(attempt);
      attempt.reject(abortError('Discord voice connection was cancelled by disconnect()'));
    }
    const errors: Error[] = [];
    if (sendGatewayLeave && shouldLeave && this.#backend?.nodeType === 'remote') {
      try {
        await this.#runtime.sendVoicePayload(this.guildId, null);
      } catch (error) {
        errors.push(asError(error));
      }
    }
    try {
      await this.#backendWork?.catch(() => undefined);
      await this.#backend?.disconnect(transportGeneration);
    } catch (error) {
      errors.push(asError(error));
    }
    this.#voiceSessionId = undefined;
    this.#voiceServer = undefined;
    this.#connectionError = errors.length ? errors.map((error) => error.message).join('; ') : null;
    this.#setConnectionState(errors.length ? 'failed' : 'disconnected');
    if (errors.length) {
      this.#emitVoiceFailure(new AggregateError(errors, 'Discord voice disconnection failed'));
      throw new AggregateError(errors, 'Discord voice disconnection failed');
    }
  }

  #handleExternalDisconnect(): void {
    this.#joinRequested = false;
    this.#voiceSessionId = undefined;
    this.#voiceServer = undefined;
    const abortBinding = this.#playAbortBinding;
    this.#clearPlayAbortBinding(abortBinding);
    (abortBinding?.controller ?? this.#playController)?.abort(
      abortError('Discord Gateway disconnected the voice session'),
    );
    const attempt = this.#attempt;
    if (attempt) {
      this.#finishAttempt(attempt);
      attempt.reject(new Error('Discord Gateway reported that the bot left the voice channel'));
    }
    this.#setConnectionState('disconnected');
    const transportGeneration = this.#nextTransportGeneration();
    void this.#backend?.disconnect(transportGeneration).catch((error: unknown) => {
      this.#connectionError = asError(error).message;
      this.#setConnectionState('failed');
      this.#emitVoiceFailure(asError(error));
    });
  }

  #failAttempt(attempt: ConnectAttempt, error: Error, leaveGateway: boolean): void {
    if (this.#attempt !== attempt) return;
    if (attempt.moving) this.#suspendRemoteVoice(attempt);
    this.#finishAttempt(attempt);
    this.#connectionError = error.name === 'AbortError' ? null : error.message;
    this.#setConnectionState(error.name === 'AbortError' ? 'disconnected' : 'failed');
    if (error.name !== 'AbortError') this.#emitVoiceFailure(error);
    if (attempt.internal && !attempt.moving && error.name !== 'AbortError') {
      const transportGeneration = this.#nextTransportGeneration();
      void this.#backend?.failVoiceRecovery?.(transportGeneration).catch(() => undefined);
    } else if (!attempt.internal && !attempt.moving && this.#backend?.nodeType === 'remote') {
      // A voice.connect RPC may have timed out before the server finished. Fence
      // that request before permitting a later join to install a stale transport.
      const transportGeneration = this.#nextTransportGeneration();
      void this.#backend.disconnect(transportGeneration).catch(() => undefined);
    }
    const shouldLeave = leaveGateway && this.#joinRequested;
    this.#joinRequested = false;
    attempt.reject(error);
    if (shouldLeave)
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, null)).catch(
        () => undefined,
      );
  }

  #retryGatewayCredentials(attempt: ConnectAttempt, error: Error): boolean {
    const retryInitial =
      !attempt.internal &&
      this.#backend?.nodeType === 'remote' &&
      error instanceof RemediaRpcError &&
      (error.code === 'transport_failed' || error.code === 'request_timeout');
    if (
      (!attempt.internal && !retryInitial) ||
      attempt.moving ||
      !this.#isCurrent(attempt) ||
      attempt.retryCount >= 2 ||
      this.#disposed
    ) {
      return false;
    }
    attempt.retryCount += 1;
    attempt.requestSent = false;
    attempt.sequenceFloor = this.#runtime.gatewaySequence();
    if (retryInitial) {
      // A terminated first session must not be reused. Require new Gateway
      // dispatches for both credentials rather than pairing a stale session ID
      // with the replacement server token.
      attempt.voiceState = undefined;
      this.#voiceSessionId = undefined;
    } else {
      attempt.voiceState ??= this.#voiceSessionId ? { sessionId: this.#voiceSessionId } : undefined;
    }
    attempt.voiceServer = undefined;
    attempt.credentialRevision += 1;
    this.#suspendRemoteVoice(attempt);
    this.#setConnectionState('reconnecting');
    void Promise.resolve(
      this.#runtime.sendVoicePayload(this.guildId, this.channelId, this.#selfDeaf, this.#selfMute),
    )
      .then(() => {
        if (!this.#isCurrent(attempt)) return;
        attempt.requestSent = true;
        this.#tryStartRemoteConnection(attempt);
      })
      .catch((sendError: unknown) => {
        this.#failAttempt(attempt, asError(sendError), true);
      });
    return true;
  }

  #finishAttempt(attempt: ConnectAttempt): void {
    if (attempt.settled) return;
    attempt.settled = true;
    clearTimeout(attempt.timer);
    if (attempt.signal && attempt.abortListener)
      attempt.signal.removeEventListener('abort', attempt.abortListener);
    if (this.#attempt === attempt) this.#attempt = undefined;
  }

  #isCurrent(attempt: ConnectAttempt): boolean {
    return !this.#disposed && this.#attempt === attempt && !attempt.settled;
  }

  async #ensureBackend(): Promise<GuildBackend> {
    if (this.#backendWork) return this.#backendWork;
    if (this.#backend) return this.#backend;
    const work = (async () => {
      let backend: GuildBackend | undefined;
      backend = await this.#runtime.resolveBackend(this.#options, () => {
        // A migrated backend must not unregister the wrapper when its old node is disposed.
        if (this.#backend !== backend) return;
        this.#backend = undefined;
        this.#runtime.releaseSession(this);
      });
      if (this.#disposed || this.#closing || this.#connectionState === 'disconnecting') {
        await backend.dispose();
        throw new Error('Discord session closed during backend allocation');
      }
      this.#backend = backend;
      await this.#serializeConfiguration(() => this.#applyConfiguration(backend));
      this.#bindBackend(backend);
      return backend;
    })();
    this.#backendWork = work;
    try {
      return await work;
    } finally {
      if (this.#backendWork === work) this.#backendWork = undefined;
    }
  }

  async #replaceFilterGraph(graph: string | null): Promise<void> {
    this.#assertOpen();
    await this.#serializeConfiguration(async () => {
      this.#assertOpen();
      if (this.#connectionState === 'disconnecting')
        throw new Error('Discord session is disconnecting');
      const previousGraph = this.#filterGraph;
      if (previousGraph === graph) return;
      if (this.#backend) {
        await this.#backend.setFilterGraph(graph);
      } else {
        this.#publishPlaybackEvent(PlaybackEvent.filterChanged, {
          sessionId: this.sessionId,
          guildId: this.guildId,
          nodeId: this.nodeId,
          generation: this.#playbackGeneration,
          previousGraph,
          graph,
        });
      }
      this.#filterGraph = graph;
    });
  }

  async #applyConfiguration(backend: GuildBackend): Promise<void> {
    if (this.#filterGraph !== null && backend.filterGraph !== this.#filterGraph) {
      await backend.setFilterGraph(this.#filterGraph);
    }
    if (backend.volume !== this.#volume) await backend.setVolume(this.#volume);
  }

  #serializeConfiguration(operation: () => Promise<void>): Promise<void> {
    const result = this.#configurationWork.then(operation, operation);
    this.#configurationWork = result.catch(() => undefined);
    return result;
  }

  #requireExistingBackend(): GuildBackend {
    this.#assertOpen();
    if (!this.#backend) throw new Error('Discord voice session has no playback backend');
    return this.#backend;
  }

  #bindBackend(backend: GuildBackend): void {
    if (this.#boundBackend === backend) return;
    this.#detachBackend();
    this.#boundBackend = backend;
    const bindPlayback = <Name extends keyof PlaybackLifecycleEventMap<unknown>>(
      name: Name,
    ): void => {
      const rawBackend = backend as unknown as EventEmitter;
      const rawListener = (payload: unknown): void => {
        this.#publishPlaybackEvent(name, payload as unknown as Record<string, unknown>);
      };
      rawBackend.on(name, rawListener);
      this.#backendUnsubscribers.push(() => rawBackend.off(name, rawListener));
    };
    bindPlayback(PlaybackEvent.playbackStarted);
    bindPlayback(PlaybackEvent.playbackCompleted);
    bindPlayback(PlaybackEvent.playbackStopped);
    bindPlayback(PlaybackEvent.playbackFailed);
    bindPlayback(PlaybackEvent.playbackPaused);
    bindPlayback(PlaybackEvent.playbackResumed);
    bindPlayback(PlaybackEvent.playbackSeeked);
    bindPlayback(PlaybackEvent.prepareRequested);
    bindPlayback(PlaybackEvent.stageReady);
    bindPlayback(PlaybackEvent.stageFailed);
    bindPlayback(PlaybackEvent.stageInvalidated);
    bindPlayback(PlaybackEvent.transitionCancelled);
    bindPlayback(PlaybackEvent.filterChanged);
    bindPlayback(PlaybackEvent.volumeChanged);

    const onVoiceState = (event: VoiceLifecycleEventMap['voiceStateChanged'][0]): void => {
      const state = event.state;
      // During an in-flight join, the attempt owns the final state. A failed
      // transport may be retried with fresh Gateway credentials, so do not
      // publish a terminal state while recovery is still possible.
      if (
        this.#attempt &&
        (state === 'connected' || state === 'failed' || state === 'disconnected')
      )
        return;
      if (this.#connectionState === state) return;
      if (
        (state === 'failed' || state === 'disconnected') &&
        backend.nodeType === 'remote' &&
        this.#joinRequested &&
        !this.#attempt
      ) {
        this.#beginCredentialRefresh(true);
        return;
      }
      const previousState = this.#connectionState;
      this.#connectionState = state;
      this.emit(VoiceEvent.voiceStateChanged, {
        sessionId: this.sessionId,
        guildId: this.guildId,
        nodeId: backend.nodeId,
        previousState,
        state,
      });
    };
    backend.on(VoiceEvent.voiceStateChanged, onVoiceState);
    this.#backendUnsubscribers.push(() => backend.off(VoiceEvent.voiceStateChanged, onVoiceState));

    const onVoiceFailed = (event: VoiceLifecycleEventMap['voiceFailed'][0]): void => {
      if (this.#attempt) return; // #failAttempt reports the final error after bounded retries.
      this.#emitVoiceFailure(event.error);
    };
    backend.on(VoiceEvent.voiceFailed, onVoiceFailed);
    this.#backendUnsubscribers.push(() => backend.off(VoiceEvent.voiceFailed, onVoiceFailed));
  }

  #detachBackend(): void {
    for (const unsubscribe of this.#backendUnsubscribers.splice(0)) unsubscribe();
    this.#boundBackend = undefined;
  }

  #publishPlaybackEvent(
    event: keyof PlaybackLifecycleEventMap<unknown>,
    payload: Record<string, unknown>,
  ): void {
    const generation =
      typeof payload.generation === 'number' && Number.isSafeInteger(payload.generation)
        ? payload.generation
        : this.#playbackGeneration;
    if (event === PlaybackEvent.playbackStarted && this.#startedGenerations.has(generation)) return;
    const terminal =
      event === PlaybackEvent.playbackCompleted ||
      event === PlaybackEvent.playbackStopped ||
      event === PlaybackEvent.playbackFailed;
    if (terminal && this.#terminalGenerations.has(generation)) return;

    const hasCustomMetadata = Object.hasOwn(payload, 'customMetadata');
    if (
      !hasCustomMetadata &&
      !this.#metadataByGeneration.has(generation) &&
      this.#pendingPlay?.metadata !== undefined &&
      generation > this.#playbackGeneration
    ) {
      const buffered = this.#earlyPlaybackEvents.get(generation) ?? [];
      if (
        buffered.length < 16 &&
        this.#earlyPlaybackEvents.size < 8 &&
        !buffered.some((item) => item.event === event)
      ) {
        buffered.push({ event, payload });
        this.#earlyPlaybackEvents.set(generation, buffered);
      }
      return;
    }

    if (event === PlaybackEvent.playbackStarted) {
      const transition = payload.transition;
      if (transition && typeof transition === 'object' && !Array.isArray(transition)) {
        const entryId = (transition as { entryId?: unknown }).entryId;
        if (typeof entryId === 'string') {
          this.#metadataByGeneration.set(generation, this.#stageMetadata.get(entryId));
          this.#stageMetadata.delete(entryId);
        }
        this.#playbackGeneration = Math.max(this.#playbackGeneration, generation);
      }
      this.#startedGenerations.add(generation);
    }
    if (event === PlaybackEvent.stageInvalidated && typeof payload.entryId === 'string') {
      this.#stageMetadata.delete(payload.entryId);
    }
    if (terminal) {
      this.#clearPlayAbortBindingForGeneration(generation);
      this.#terminalGenerations.add(generation);
      while (this.#terminalGenerations.size > 16) {
        const oldest = this.#terminalGenerations.values().next().value;
        if (oldest === undefined) break;
        this.#terminalGenerations.delete(oldest);
      }
    }

    const metadata = hasCustomMetadata
      ? payload.customMetadata
      : this.#metadataByGeneration.get(generation);
    const enriched: Record<string, unknown> = {
      ...payload,
      sessionId: this.sessionId,
      guildId: this.guildId,
      nodeId: this.nodeId,
      generation,
    };
    if (metadata !== undefined) enriched.customMetadata = metadata;
    else delete enriched.customMetadata;
    this.#emitTypedPlaybackEvent(event, enriched);
    if (terminal) {
      this.#metadataByGeneration.delete(generation);
      this.#startedGenerations.delete(generation);
      if (generation === this.#playbackGeneration) {
        this.#migratableSource = undefined;
        this.#migratableMetadata = undefined;
      }
    }
  }

  #emitTypedPlaybackEvent(
    event: keyof PlaybackLifecycleEventMap<Metadata>,
    payload: Record<string, unknown>,
  ): void {
    (this as unknown as EventEmitter).emit(event, payload);
  }

  #clearPlayAbortBinding(binding = this.#playAbortBinding): void {
    if (!binding || this.#playAbortBinding !== binding) return;
    binding.signal.removeEventListener('abort', binding.listener);
    this.#playAbortBinding = undefined;
  }

  #clearPlayAbortBindingForGeneration(generation: number): void {
    const binding = this.#playAbortBinding;
    if (binding?.generation === generation) this.#clearPlayAbortBinding(binding);
  }

  #flushEarlyEvents(generation: number): void {
    const events = this.#earlyPlaybackEvents.get(generation) ?? [];
    this.#earlyPlaybackEvents.delete(generation);
    for (const item of events) this.#publishPlaybackEvent(item.event, item.payload);
  }

  #flushPendingFailure(): void {
    if (!this.#pendingPlay) return;
    const generation = [...this.#earlyPlaybackEvents.entries()]
      .filter(([, events]) => events.some((event) => event.event === PlaybackEvent.playbackFailed))
      .map(([candidate]) => candidate)
      .sort((left, right) => left - right)[0];
    if (generation === undefined) {
      this.#earlyPlaybackEvents.clear();
      return;
    }
    this.#metadataByGeneration.set(generation, this.#pendingPlay.metadata);
    this.#playbackGeneration = Math.max(this.#playbackGeneration, generation);
    this.#flushEarlyEvents(generation);
    this.#earlyPlaybackEvents.clear();
  }

  #boundMetadata(currentGeneration: number): void {
    while (this.#metadataByGeneration.size > 8) {
      const oldest = this.#metadataByGeneration.keys().next().value;
      if (oldest === undefined || oldest === currentGeneration) break;
      this.#metadataByGeneration.delete(oldest);
    }
  }

  #emitVoiceFailure(error: Error): void {
    if (this.#voiceFailureEmitted) return;
    this.#voiceFailureEmitted = true;
    this.emit(VoiceEvent.voiceFailed, {
      sessionId: this.sessionId,
      guildId: this.guildId,
      nodeId: this.nodeId,
      state: 'failed',
      error,
    });
  }

  #setConnectionState(state: VoiceConnectionState): void {
    if (this.#connectionState === state) return;
    const previousState = this.#connectionState;
    this.#connectionState = state;
    this.emit(VoiceEvent.voiceStateChanged, {
      sessionId: this.sessionId,
      guildId: this.guildId,
      nodeId: this.nodeId,
      previousState,
      state,
    });
  }

  #assertOpen(): void {
    if (this.#disposed || this.#closing) throw new Error('Discord session has been disposed');
    if (this.#sessionLossReason) {
      throw new SessionLostError(
        this.#backend?.nodeId ?? 'unknown',
        [this.guildId],
        this.#sessionLossReason,
      );
    }
  }
}

/**
 * Parse a relevant raw Gateway voice dispatch; unrelated or malformed packets are ignored.
 * @param packet - Raw packet from this bot's Gateway dispatch stream.
 * @returns A guild-scoped voice dispatch, or undefined when the packet is not relevant.
 */
export function parseGatewayDispatch(packet: DiscordGatewayPacket):
  | {
      event: string;
      data: Record<string, unknown>;
      sequence?: number;
    }
  | undefined {
  if (typeof packet.t !== 'string' || !isRecord(packet.d)) return undefined;
  if (packet.t !== 'VOICE_STATE_UPDATE' && packet.t !== 'VOICE_SERVER_UPDATE') return undefined;
  const guildId = packet.d.guild_id;
  if (typeof guildId !== 'string' || !isSnowflake(guildId)) return undefined;
  const dispatch: { event: string; data: Record<string, unknown>; sequence?: number } = {
    event: packet.t,
    data: packet.d,
  };
  if (typeof packet.s === 'number' && Number.isSafeInteger(packet.s) && packet.s >= 0)
    dispatch.sequence = packet.s;
  return dispatch;
}

function validateSnowflake(name: string, value: string): string {
  if (!isSnowflake(value))
    throw new TypeError(`${name} must be a positive decimal Discord snowflake string`);
  return value;
}

function isSnowflake(value: unknown): value is string {
  if (typeof value !== 'string' || !/^\d{1,20}$/.test(value)) return false;
  try {
    return BigInt(value) > 0n && BigInt(value) <= 18_446_744_073_709_551_615n;
  } catch {
    return false;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function emptyPosition(): PlaybackPosition {
  return { mediaMs: null, mediaPositionAccuracy: null, submittedMs: 0, playedMs: null };
}

function emptySnapshot(filterGraph: string | null, volume: number): PlaybackSnapshot {
  return {
    state: 'idle',
    generation: 0,
    processingMode: null,
    position: emptyPosition(),
    durationMs: null,
    source: null,
    filterGraph,
    volume,
    lastError: null,
  };
}

function abortError(message: string): Error {
  return new DOMException(message, 'AbortError');
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

function isPlaybackTerminal(state: PlaybackState): boolean {
  return state === 'completed' || state === 'failed' || state === 'stopped';
}
