import { randomUUID } from 'node:crypto';
import { RemediaEventEmitter } from './events.js';
import { NativeRemedia, type NativeVoiceOptions, type NativeVoiceSessionIds } from '../native.js';
import { DiscordGatewayAdapter, isSnowflake } from './gateway.js';
import {
  PlaybackHooks,
  PlaybackSessionEvents as PlaybackEvent,
  VoiceSessionEvents as VoiceEvent,
  type LifecycleVoiceState,
  type PlayOptions,
  type PlaybackSessionEventMap,
  type SessionLifecycleEvents,
  type StageNextOptions,
  type StageSnapshot,
} from './lifecycle.js';
import { PlaybackSession } from './playback.js';
import { FilterControls, validateVolume } from './filters.js';
import type { MediaSource } from './input.js';
import type {
  AudioStreamInfo,
  PlaybackPosition,
  PlaybackState,
  VoiceConnectionState,
} from './types.js';

const DEFAULT_CONNECT_TIMEOUT_MS = 45_000;

/**
 * Identity, initial playback configuration, and Gateway voice flags for one guild session. The channel is a mutable destination, while userId identifies the bot.
 */
export interface DiscordSessionOptions {
  /** Discord server snowflake as a decimal string. */
  guildId: string;
  /** Voice channel snowflake as a decimal string. */
  channelId: string;
  /** Bot user snowflake as a decimal string. */
  userId: string;
  /** Discord Gateway self-deaf state; defaults to true. */
  selfDeaf?: boolean;
  /** Discord Gateway self-mute state; defaults to false. */
  selfMute?: boolean;
  /** Session master linear gain, finite from 0 through 2; defaults to 1. */
  volume?: number;
  /** Trusted raw FFmpeg audio filtergraph; defaults to null. Native syntax validation occurs during backend configuration before playback. */
  filterGraph?: string | null;
  crossfade?: { enabled: true; durationMs?: number; prepareBeforeMs?: number } | false;
}

/** Cancellation and timeout controls for obtaining credentials and establishing a voice transport. */
export interface DiscordConnectOptions {
  /** Optional signal that cancels Gateway negotiation and pending transport setup. */
  signal?: AbortSignal;
  /** Maximum time to receive Gateway credentials and establish voice transport/DAVE. */
  timeoutMs?: number;
}

interface VoiceStateCredentials {
  sessionId: string;
}

interface VoiceServerCredentials {
  token: string;
  endpoint: string;
}

interface VoiceCredentials extends VoiceStateCredentials, VoiceServerCredentials {}

interface ConnectAttempt {
  generation: number;
  sequenceFloor: number | undefined;
  promise: Promise<void>;
  resolve: () => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
  signal: AbortSignal | undefined;
  abortListener: (() => void) | undefined;
  requestSent: boolean;
  settled: boolean;
  internal: boolean;
  moving?: boolean;
  retryCount: number;
  transportGeneration: number | undefined;
}

/**
 * One Discord voice session. The caller supplies a Gateway send adapter to `Remedia.create()` and
 * forwards raw dispatch packets to `Remedia.receive()`; this session collects the matching voice
 * credentials and opens the native transport after both dispatches arrive.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export class DiscordSession<Metadata = unknown> extends RemediaEventEmitter<
  SessionLifecycleEvents<Metadata>
> {
  /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
  readonly sessionId = randomUUID();
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  readonly guildId: string;
  #channelId: string;
  #selfDeaf: boolean;
  #selfMute: boolean;
  #closing = false;
  #disposeWork: Promise<void> | undefined;
  /** Discord bot user snowflake as a decimal string; separate from Remedia session identity. */
  readonly userId: string;

  readonly #native: NativeRemedia;
  readonly #gateway: DiscordGatewayAdapter;
  readonly #onDispose: () => void;
  /** Persistent raw FFmpeg filtergraph controls for this session. */
  readonly filters: FilterControls;
  /** Ordered, awaited local pre-play hooks; listeners are separate from hooks. */
  readonly hooks = new PlaybackHooks<Metadata>();
  #playback: PlaybackSession<Metadata> | undefined;
  #playbackSubscriptions: { playback: PlaybackSession<Metadata>; detach: () => void } | undefined;
  #nativeSession: NativeVoiceSessionIds | undefined;
  #attempt: ConnectAttempt | undefined;
  #voiceState: VoiceStateCredentials | undefined;
  #voiceServer: VoiceServerCredentials | undefined;
  #voiceServerUnavailable = false;
  #nativeTransportGeneration = 0;
  #gatewayEpoch: number | undefined;
  #lastGatewaySequence: number | undefined;
  #nativeWork: Promise<void> = Promise.resolve();
  #backgroundCleanup: Promise<void> = Promise.resolve();
  #disconnectPromise: Promise<void> | undefined;
  #connectionState: VoiceConnectionState = 'disconnected';
  #connectionError: string | undefined;
  #generation = 0;
  #playbackGeneration = 0;
  #joinRequested = false;
  #disposed = false;
  #filterGraph: string | null = null;
  #volume = 1;
  #crossfade: DiscordSessionOptions['crossfade'];
  #configurationWork: Promise<void> = Promise.resolve();
  #nativeStateTimer: ReturnType<typeof setInterval> | undefined;

  /**
   * @internal Constructed by `Remedia.createDiscordSession()`.
   * @param native - Owning native engine instance; its resources must outlive this wrapper.
   * @param gateway - Adapter for the application-owned Discord Gateway connection.
   * @param options - Configuration and lifecycle options for this operation.
   * @param onDispose - Callback invoked when this wrapper releases its owning registration.
   */
  constructor(
    native: NativeRemedia,
    gateway: DiscordGatewayAdapter,
    options: DiscordSessionOptions,
    onDispose: () => void,
  ) {
    super();
    this.guildId = validateSnowflake('guildId', options.guildId);
    this.#channelId = validateSnowflake('channelId', options.channelId);
    this.userId = validateSnowflake('userId', options.userId);
    validateVolume(options.volume ?? 1);
    this.#crossfade = options.crossfade;
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
    this.#native = native;
    this.#gateway = gateway;
    this.#onDispose = onDispose;
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
   * Replace only the voice transport. A newer move supersedes a pending move.
   * @param channelId - Destination Discord voice channel snowflake as a positive decimal string.
   * @returns Resolves when the requested destination connects; rejects on supersession, cancellation, or connection failure.
   */
  move(channelId: string): Promise<void> {
    this.#assertNotDisposed();
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
    if (!this.#nativeSession) return this.connect();
    const attempt = this.#createAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true);
    attempt.moving = true;
    this.#attempt = attempt;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    this.#voiceServerUnavailable = false;
    this.#setConnectionState('connecting');
    this.#suspendCurrentNativeTransport(attempt);
    this.#requestGatewayJoin(attempt);
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
    this.#assertNotDisposed();
    if (typeof enabled !== 'boolean') throw new TypeError('voice flag must be boolean');
    await this.#serializeConfiguration(async () => {
      this.#assertNotDisposed();
      if (this.#connectionState === 'disconnecting')
        throw new Error('Discord session is disconnecting');
      if ((flag === 'deaf' ? this.#selfDeaf : this.#selfMute) === enabled) return;
      if (this.#joinRequested)
        await this.#gateway.setVoiceChannel(
          this.guildId,
          this.channelId,
          flag === 'deaf' ? enabled : this.#selfDeaf,
          flag === 'mute' ? enabled : this.#selfMute,
        );
      if (flag === 'deaf') this.#selfDeaf = enabled;
      else this.#selfMute = enabled;
    });
  }

  /** Current voice transport state. A connected voice WebSocket may temporarily be reconnecting. */
  get connectionState(): VoiceConnectionState {
    if (this.#disposed) return 'disconnected';
    if (this.#connectionState === 'connected' && this.#nativeSession) {
      try {
        return this.#native.voiceState(this.#nativeSession.voiceId) as VoiceConnectionState;
      } catch {
        return this.#connectionState;
      }
    }
    return this.#connectionState;
  }

  /** Most recent Gateway or native transport failure, if any. */
  get connectionError(): string | null {
    if (this.#connectionError) return this.#connectionError;
    if (!this.#nativeSession) return null;
    try {
      return this.#native.voiceError(this.#nativeSession.voiceId) ?? null;
    } catch {
      return null;
    }
  }

  /** Current playback lifecycle state. */
  get state(): PlaybackState {
    if (this.#playback) return this.#playback.state;
    return this.connectionState === 'failed' ? 'failed' : 'idle';
  }

  /** Latest source and output timeline measurements in milliseconds. */
  get position(): PlaybackPosition {
    return (
      this.#playback?.position ?? {
        mediaMs: null,
        mediaPositionAccuracy: null,
        submittedMs: 0,
        playedMs: null,
      }
    );
  }

  /** Source duration in milliseconds, or null when unknown. */
  get durationMs(): number | null {
    return this.#playback?.durationMs ?? null;
  }

  /** Most recent playback or voice failure message, or null when no error is available. */
  get error(): string | null {
    return this.#playback?.error ?? this.connectionError;
  }

  /** Current normalized linear master gain, from 0.0 (mute) through 2.0. */
  get volume(): number {
    return this.#volume;
  }

  /** Current native source generation, useful to adapters that preserve event correlation. */
  get generation(): number {
    return this.#playback?.currentGeneration ?? this.#playbackGeneration;
  }

  /**
   * Set master gain independently of this session's configured filtergraph.
   * @param gain - Finite linear master gain from 0 through 2; 1 leaves amplitude unchanged.
   * @returns Resolves after the gain is stored or applied; rejects for non-finite gain or values outside 0 through 2.
   * @throws {RangeError} If gain is non-finite or outside the inclusive range 0 through 2.
   */
  async setVolume(gain: number): Promise<void> {
    this.#assertNotDisposed();
    validateVolume(gain);
    await this.#serializeConfiguration(async () => {
      if (this.#connectionState === 'disconnecting') {
        throw new Error('Discord voice session is disconnecting');
      }
      const previousVolume = this.#volume;
      if (previousVolume === gain) return;
      if (this.#playback) {
        await this.#playback.setVolume(gain);
      } else {
        this.emit(PlaybackEvent.volumeChanged, {
          sessionId: this.sessionId,
          guildId: this.guildId,
          nodeId: null,
          generation: this.#playbackGeneration,
          previousVolume,
          volume: gain,
        });
      }
      this.#volume = gain;
    });
  }

  /**
   * Request a Gateway voice join and wait until both voice dispatches have arrived and the native
   * voice transport has completed its DAVE setup. Repeated calls while connecting share a promise.
   * @param options - Optional cancellation signal and positive timeout in milliseconds; defaults to 45000.
   * @returns Resolves when connected; rejects on timeout, cancellation, session loss, or transport failure.
   */
  connect(options: DiscordConnectOptions = {}): Promise<void> {
    this.#assertNotDisposed();
    if (this.#attempt) return this.#attempt.promise;
    const currentState = this.connectionState;
    if (currentState === 'connected' || currentState === 'reconnecting') return Promise.resolve();
    if (this.#connectionState === 'disconnecting') {
      return Promise.reject(new Error('Discord voice session is disconnecting'));
    }
    if (options.signal?.aborted) {
      return Promise.reject(abortError('Discord voice connection was cancelled'));
    }

    const timeoutMs = options.timeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS;
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
      return Promise.reject(new RangeError('timeoutMs must be a positive finite number'));
    }

    const attempt = this.#createAttempt(timeoutMs, options.signal, false);
    this.#attempt = attempt;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    this.#voiceServerUnavailable = false;
    this.#connectionError = undefined;
    this.#setConnectionState('connecting');
    this.#joinRequested = true;

    if (attempt.signal) {
      attempt.abortListener = () => {
        this.#failAttempt(attempt, abortError('Discord voice connection was cancelled'), true);
      };
      attempt.signal.addEventListener('abort', attempt.abortListener, { once: true });
    }

    this.#requestGatewayJoin(attempt);
    return attempt.promise;
  }

  /**
   * Begin playback on this connected voice session. The decoder and Opus encoder remain in core;
   * this object only associates their output with the native Discord transport.
   * @param source - Encoded media input; Buffer and caller-owned binary Readable inputs require embedded mode.
   * @param options - Optional metadata for this generation and a cancellation signal.
   * @returns Selected stream metadata once startup succeeds; rejects if hooks, input opening, or playback startup fail.
   */
  async play(source: MediaSource, options: PlayOptions<Metadata> = {}): Promise<AudioStreamInfo> {
    await this.#configurationWork;
    return this.#requirePlayback()
      .play(source, options)
      .then((info) => {
        this.#playbackGeneration = this.#playback?.currentGeneration ?? this.#playbackGeneration;
        return info;
      });
  }

  /**
   * Pause the current playback without replacing its decoder or source generation.
   * @returns Resolves when the backend accepts the pause request.
   */
  async stageNext(source: MediaSource, options: StageNextOptions<Metadata>): Promise<StageSnapshot> {
    await this.#configurationWork;
    return this.#requireExistingPlayback().stageNext(source, options);
  }

  clearStage(): Promise<void> {
    return this.#requireExistingPlayback().clearStage();
  }

  get stagedSource(): StageSnapshot | null {
    return this.#playback?.stagedSource ?? null;
  }

  pause(): Promise<void> {
    return this.#requireExistingPlayback().pause();
  }

  /**
   * Resume paused playback on its existing source generation.
   * @returns Resolves when the backend accepts the resume request.
   */
  resume(): Promise<void> {
    return this.#requireExistingPlayback().resume();
  }

  /**
   * Seek on the original media timeline; non-seekable and one-shot sources may reject.
   * @param positionMs - Non-negative finite position on the original media timeline, in milliseconds.
   * @returns Resolves after the backend accepts the seek; rejects for invalid positions or unsupported inputs.
   */
  seek(positionMs: number): Promise<void> {
    return this.#requireExistingPlayback().seek(positionMs);
  }

  /**
   * Stop the active playback generation and release its input without leaving the voice channel.
   * @returns Resolves after the backend stops the active source.
   */
  stop(): Promise<void> {
    return this.#requireExistingPlayback().stop();
  }

  /**
   * Wait for the current playback generation to reach a terminal state.
   * @returns The terminal playback state for the generation captured by this wait.
   */
  wait(): Promise<PlaybackState> {
    return this.#requireExistingPlayback().wait();
  }

  /**
   * Leave the Discord channel through Gateway OP 4 and close the native transport. This does not
   * dispose the wrapper, so `connect()` can be called again with fresh Gateway dispatches.
   * @returns Resolves after cleanup; concurrent disconnects share cleanup work.
   */
  disconnect(): Promise<void> {
    if (this.#disposed) return Promise.resolve();
    if (this.#disconnectPromise) return this.#disconnectPromise;

    const disconnect = this.#disconnect();
    this.#disconnectPromise = disconnect.finally(() => {
      this.#disconnectPromise = undefined;
    });
    return this.#disconnectPromise;
  }

  /**
   * Disconnect and unregister this session.
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
    try {
      await this.disconnect();
    } finally {
      this.#disposed = true;
      this.emit(PlaybackEvent.sessionDisposed, {
        sessionId: this.sessionId,
        guildId: this.guildId,
        nodeId: null,
        generation: this.#playbackGeneration,
      });
      this.#onDispose();
    }
  }

  /**
   * @internal Routes a parsed raw Gateway dispatch to this guild's active voice lifecycle.
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
    if (this.#disposed || data.guild_id !== this.guildId) return;
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
    ) {
      return;
    }

    if (event === 'VOICE_STATE_UPDATE') {
      this.#receiveVoiceState(data);
    } else if (event === 'VOICE_SERVER_UPDATE') {
      this.#receiveVoiceServer(data);
    }
  }

  async #disconnect(): Promise<void> {
    const attempt = this.#attempt;
    const shouldLeaveGateway = this.#joinRequested || this.#nativeSession !== undefined;
    this.#joinRequested = false;
    this.#generation += 1;
    this.#stopNativeStateMonitor();
    this.#voiceServerUnavailable = false;
    this.#invalidateNativeTransport();
    this.#setConnectionState('disconnecting');
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    if (attempt) {
      this.#finishAttempt(attempt);
      attempt.reject(abortError('Discord voice connection was cancelled by disconnect()'));
    }

    const errors: Error[] = [];
    if (shouldLeaveGateway) {
      try {
        await this.#gateway.setVoiceChannel(this.guildId, null);
      } catch (error) {
        errors.push(asError(error));
      }
    }

    // A native connection attempt cannot be interrupted inside FFmpeg/transport setup. Its own
    // network operations have finite timeouts; wait for its generation check to dispose late
    // results before returning from disconnect.
    await this.#nativeWork;
    await this.#backgroundCleanup;
    try {
      await this.#closeCurrentNativeSession();
    } catch (error) {
      errors.push(asError(error));
    }

    this.#joinRequested = false;
    this.#setConnectionState('disconnected');
    if (errors.length > 0) {
      this.#connectionError = errors.map((error) => error.message).join('; ');
      throw new AggregateError(errors, 'Discord voice disconnection failed');
    }
  }

  #receiveVoiceState(data: Record<string, unknown>): void {
    if (
      data.user_id !== this.userId ||
      !this.#joinRequested ||
      this.#connectionState === 'disconnecting'
    ) {
      return;
    }
    if (data.channel_id === null) {
      if (this.#attempt?.moving) return;
      if (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting') {
        this.#handleGatewayDisconnect();
      }
      // A null channel can be a delayed state from a previous leave while a new join is pending.
      return;
    }
    if (data.channel_id !== this.channelId || typeof data.session_id !== 'string') return;
    if (data.session_id.length === 0) return;

    const previousSessionId = this.#voiceState?.sessionId;
    if (previousSessionId === data.session_id) return;
    const refreshRequired =
      !this.#attempt &&
      (this.#connectionState === 'connected' ||
        this.#connectionState === 'reconnecting' ||
        this.#connectionState === 'disconnected' ||
        this.#connectionState === 'failed');
    this.#voiceState = { sessionId: data.session_id };
    if (previousSessionId !== undefined) {
      this.#voiceServer = undefined;
      this.#voiceServerUnavailable = true;
    }
    if (refreshRequired) {
      this.#beginCredentialRefresh();
      return;
    }
    this.#credentialsChanged();
  }

  #receiveVoiceServer(data: Record<string, unknown>): void {
    if (!this.#joinRequested || this.#connectionState === 'disconnecting') return;
    if (data.endpoint === null) {
      this.#voiceServerUnavailable = true;
      if (
        !this.#attempt &&
        ['connected', 'reconnecting', 'disconnected', 'failed'].includes(this.#connectionState)
      ) {
        this.#beginCredentialRefresh();
      } else if (this.#attempt && !this.#attempt.internal) {
        this.#failGateway(new Error('Discord did not provide a voice server endpoint'));
      }
      return;
    }
    if (typeof data.token !== 'string' || typeof data.endpoint !== 'string') return;
    if (data.token.length === 0 || data.endpoint.length === 0) return;

    const sameServer =
      this.#voiceServer?.token === data.token && this.#voiceServer.endpoint === data.endpoint;
    const currentState = this.connectionState;
    const refreshRequired =
      !this.#attempt &&
      currentState !== 'connecting' &&
      currentState !== 'negotiatingDave' &&
      (this.#voiceServerUnavailable || currentState !== 'connected' || !sameServer);
    this.#voiceServerUnavailable = false;
    if (sameServer && !refreshRequired) {
      this.#credentialsChanged();
      return;
    }
    this.#voiceServer = { token: data.token, endpoint: data.endpoint };
    if (refreshRequired) {
      this.#beginCredentialRefresh();
      return;
    }
    this.#credentialsChanged();
  }

  #credentialsChanged(): void {
    const attempt = this.#attempt;
    if (
      !attempt ||
      !this.#voiceState ||
      !this.#voiceServer ||
      this.#voiceServerUnavailable ||
      !attempt.requestSent
    )
      return;
    this.#tryStartNativeConnection(attempt);
  }

  #createAttempt(
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
    void promise.catch(() => {});
    let attempt!: ConnectAttempt;
    const onTimeout = (): void => {
      const error = new Error(`Discord voice connection timed out after ${timeoutMs} ms`);
      if (this.#retryCredentialRefresh(attempt, error)) {
        attempt.timer = setTimeout(onTimeout, timeoutMs);
      } else {
        this.#failAttempt(attempt, error, true);
      }
    };
    this.#generation += 1;
    attempt = {
      generation: this.#generation,
      sequenceFloor: this.#gateway.lastSequence,
      promise,
      resolve,
      reject,
      timer: setTimeout(onTimeout, timeoutMs),
      signal,
      abortListener: undefined,
      requestSent: false,
      settled: false,
      internal,
      retryCount: 0,
      transportGeneration: undefined,
    };
    return attempt;
  }

  #requestGatewayJoin(attempt: ConnectAttempt): void {
    let sendPromise: void | Promise<void>;
    try {
      sendPromise = this.#gateway.setVoiceChannel(
        this.guildId,
        this.channelId,
        this.#selfDeaf,
        this.#selfMute,
      );
    } catch (error) {
      if (!this.#retryCredentialRefresh(attempt, asError(error))) {
        this.#failAttempt(attempt, asError(error), true);
      }
      return;
    }
    void Promise.resolve(sendPromise)
      .then(() => {
        if (this.#attempt !== attempt) return;
        attempt.requestSent = true;
        this.#tryStartNativeConnection(attempt);
      })
      .catch((error: unknown) => {
        if (!this.#retryCredentialRefresh(attempt, asError(error))) {
          this.#failAttempt(attempt, asError(error), true);
        }
      });
  }

  #beginCredentialRefresh(requireFreshServer = false): void {
    if (this.#attempt || this.#disposed) return;
    if (requireFreshServer) {
      this.#voiceServer = undefined;
      this.#voiceServerUnavailable = true;
    }
    const attempt = this.#createAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true);
    // This internal attempt may have no direct caller waiting on its Promise.
    void attempt.promise.catch(() => {});
    this.#attempt = attempt;
    this.#setConnectionState('reconnecting');
    this.#connectionError = undefined;
    this.#joinRequested = true;
    this.#suspendCurrentNativeTransport(attempt);
    this.#requestGatewayJoin(attempt);
  }

  #tryStartNativeConnection(attempt: ConnectAttempt): void {
    if (
      this.#attempt !== attempt ||
      !attempt.requestSent ||
      !this.#voiceState ||
      !this.#voiceServer ||
      this.#voiceServerUnavailable
    ) {
      return;
    }
    this.#scheduleNativeConnection(attempt);
  }

  #scheduleNativeConnection(attempt: ConnectAttempt | undefined): void {
    if (!this.#voiceState || !this.#voiceServer) return;
    const credentials: VoiceCredentials = {
      ...this.#voiceState,
      ...this.#voiceServer,
    };
    const generation = ++this.#generation;
    if (attempt) attempt.generation = generation;
    const existing = this.#nativeSession;
    const transportGeneration = existing ? this.#nextNativeTransportGeneration() : undefined;
    if (existing && transportGeneration !== undefined) {
      if (attempt) attempt.transportGeneration = transportGeneration;
      try {
        this.#native.invalidateVoiceRecovery(existing.voiceId, transportGeneration);
      } catch (error) {
        this.#nativeConnectionFailed(asError(error), generation, attempt);
        return;
      }
    }
    this.#setConnectionState(this.#nativeSession ? 'reconnecting' : 'negotiatingDave');

    const work = this.#nativeWork.then(async () => {
      if (!this.#isCurrent(generation, attempt)) return;
      await this.#backgroundCleanup;
      if (!this.#isCurrent(generation, attempt)) return;
      const options: NativeVoiceOptions = {
        guildId: this.guildId,
        channelId: this.channelId,
        userId: this.userId,
        sessionId: credentials.sessionId,
        token: credentials.token,
        endpoint: credentials.endpoint,
      };
      let playback: PlaybackSession<Metadata> | undefined;
      if (existing) {
        if (transportGeneration === undefined) {
          throw new Error('Discord voice replacement generation is unavailable');
        }
        await this.#native.replaceVoice(existing.voiceId, transportGeneration, options);
        if (!this.#isCurrent(generation, attempt)) return;
        playback = this.#playback;
      } else {
        const nativeSession = await this.#native.connectVoice(options);
        if (!this.#isCurrent(generation, attempt)) {
          await this.#releaseNativeSession(nativeSession, undefined);
          return;
        }
        this.#nativeSession = nativeSession;
        const createdPlayback = new PlaybackSession<Metadata>(
          this.#native,
          nativeSession.playbackId,
          undefined,
          this.guildId,
          this.hooks,
        );
        playback = createdPlayback;
        this.#playback = createdPlayback;
        if (this.#crossfade) createdPlayback.configureCrossfade(this.#crossfade);
        await this.#serializeConfiguration(async () => {
          if (this.#playback !== createdPlayback) return;
          if (this.#filterGraph !== null) await createdPlayback.filters.set(this.#filterGraph);
          if (this.#volume !== 1) await createdPlayback.setVolume(this.#volume);
        });
        if (!this.#isCurrent(generation, attempt)) return;
        this.#forwardPlaybackEvents(createdPlayback);
      }
      this.#connectionError = undefined;
      this.#setConnectionState('connected');
      this.#voiceState = { sessionId: credentials.sessionId };
      this.#voiceServer = { token: credentials.token, endpoint: credentials.endpoint };
      this.#voiceServerUnavailable = false;
      this.#startNativeStateMonitor();
      this.#playbackGeneration = playback?.currentGeneration ?? this.#playbackGeneration;
      if (attempt && this.#attempt === attempt) {
        this.#finishAttempt(attempt);
        attempt.resolve();
      }
    });

    this.#nativeWork = work.catch((error: unknown) => {
      this.#nativeConnectionFailed(asError(error), generation, attempt);
    });
  }

  #suspendCurrentNativeTransport(attempt: ConnectAttempt): void {
    const nativeSession = this.#nativeSession;
    if (!nativeSession) return;
    const generation = this.#nextNativeTransportGeneration();
    attempt.transportGeneration = generation;
    try {
      this.#native.invalidateVoiceRecovery(nativeSession.voiceId, generation);
    } catch (error) {
      this.#nativeConnectionFailed(asError(error), attempt.generation, attempt);
      return;
    }
    const work = this.#nativeWork.then(() =>
      this.#native.suspendVoice(nativeSession.voiceId, generation),
    );
    this.#nativeWork = work.catch((error: unknown) => {
      if (this.#isCurrent(attempt.generation, attempt)) {
        this.#connectionError = asError(error).message;
      }
    });
  }

  #startNativeStateMonitor(): void {
    this.#stopNativeStateMonitor();
    this.#nativeStateTimer = setInterval(() => {
      if (this.#disposed || !this.#nativeSession || this.#attempt || !this.#joinRequested) return;
      try {
        const state = this.#native.voiceState(this.#nativeSession.voiceId);
        if (state === 'failed' || state === 'disconnected') {
          this.#beginCredentialRefresh(true);
        }
      } catch {
        // Explicit close/disposal owns cleanup if the native entry disappeared.
      }
    }, 250);
    this.#nativeStateTimer.unref?.();
  }

  #stopNativeStateMonitor(): void {
    if (!this.#nativeStateTimer) return;
    clearInterval(this.#nativeStateTimer);
    this.#nativeStateTimer = undefined;
  }

  #invalidateNativeTransport(): void {
    const nativeSession = this.#nativeSession;
    if (!nativeSession) return;
    try {
      this.#native.invalidateVoiceRecovery(
        nativeSession.voiceId,
        this.#nextNativeTransportGeneration(),
      );
    } catch {
      // Explicit disconnect and disposal remain authoritative while native cleanup runs.
    }
  }

  #nextNativeTransportGeneration(): number {
    this.#nativeTransportGeneration =
      this.#nativeTransportGeneration >= 0xffff_fffe ? 1 : this.#nativeTransportGeneration + 1;
    return this.#nativeTransportGeneration;
  }

  #isCurrent(generation: number, attempt: ConnectAttempt | undefined): boolean {
    return (
      !this.#disposed &&
      this.#generation === generation &&
      (attempt ? this.#attempt === attempt : this.#attempt === undefined)
    );
  }

  #nativeConnectionFailed(
    error: Error,
    generation: number,
    attempt: ConnectAttempt | undefined,
  ): void {
    if (!this.#isCurrent(generation, attempt)) return;
    if (attempt && this.#retryCredentialRefresh(attempt, error)) return;
    this.#connectionError = error.message;
    this.#stopNativeStateMonitor();
    this.#setConnectionState('failed');
    const pending = this.#attempt;
    if (pending) {
      this.#finishAttempt(pending);
      pending.reject(error);
    }
    const shouldLeaveGateway = this.#joinRequested;
    this.#joinRequested = false;
    this.#generation += 1;
    if (!attempt?.internal) {
      this.#voiceState = undefined;
      this.#voiceServer = undefined;
      this.#voiceServerUnavailable = false;
    }
    if (attempt?.moving) {
      // Keep playback suspended for an explicit retry or disconnect.
    } else if (attempt?.internal && this.#nativeSession) {
      try {
        this.#native.failVoiceRecovery(
          this.#nativeSession.voiceId,
          'Discord voice recovery failed',
        );
      } catch {
        // A concurrently disposed native instance already released the router.
      }
    } else {
      this.#queueCurrentNativeCleanup();
    }
    if (shouldLeaveGateway) void this.#sendGatewayLeave();
  }

  #failGateway(error: Error): void {
    const attempt = this.#attempt;
    if (attempt) {
      if (!this.#retryCredentialRefresh(attempt, error)) this.#failAttempt(attempt, error, true);
      return;
    }
    if (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting') {
      this.#nativeConnectionFailed(error, this.#generation, undefined);
    }
  }

  #failAttempt(attempt: ConnectAttempt, error: Error, leaveGateway: boolean): void {
    if (this.#attempt !== attempt) return;
    if (attempt.moving) this.#suspendCurrentNativeTransport(attempt);
    this.#generation += 1;
    this.#finishAttempt(attempt);
    this.#connectionError = error.name === 'AbortError' ? undefined : error.message;
    this.#stopNativeStateMonitor();
    this.#setConnectionState(error.name === 'AbortError' ? 'disconnected' : 'failed');
    if (!attempt.internal) {
      this.#voiceState = undefined;
      this.#voiceServer = undefined;
      this.#voiceServerUnavailable = false;
    }
    if (attempt.moving) {
      // Keep playback suspended for an explicit retry or disconnect.
    } else if (attempt.internal && this.#nativeSession) {
      try {
        this.#native.failVoiceRecovery(
          this.#nativeSession.voiceId,
          'Discord voice recovery timed out',
        );
      } catch {
        // The owning Remedia instance may already be closing.
      }
    } else {
      this.#queueCurrentNativeCleanup();
    }
    const shouldLeaveGateway = leaveGateway && this.#joinRequested;
    this.#joinRequested = false;
    attempt.reject(error);
    if (shouldLeaveGateway) void this.#sendGatewayLeave();
  }

  #retryCredentialRefresh(attempt: ConnectAttempt, _error: Error): boolean {
    if (
      !attempt.internal ||
      attempt.moving ||
      this.#attempt !== attempt ||
      attempt.settled ||
      attempt.retryCount >= 2 ||
      this.#disposed
    ) {
      return false;
    }
    attempt.retryCount += 1;
    this.#generation += 1;
    attempt.generation = this.#generation;
    attempt.sequenceFloor = this.#gateway.lastSequence;
    attempt.requestSent = false;
    this.#voiceServer = undefined;
    this.#voiceServerUnavailable = true;
    this.#setConnectionState('reconnecting');
    this.#suspendCurrentNativeTransport(attempt);
    this.#requestGatewayJoin(attempt);
    return true;
  }

  #finishAttempt(attempt: ConnectAttempt): void {
    if (attempt.settled) return;
    attempt.settled = true;
    clearTimeout(attempt.timer);
    if (attempt.signal && attempt.abortListener) {
      attempt.signal.removeEventListener('abort', attempt.abortListener);
    }
    if (this.#attempt === attempt) this.#attempt = undefined;
  }

  async #handleGatewayDisconnect(): Promise<void> {
    const attempt = this.#attempt;
    this.#generation += 1;
    this.#setConnectionState('disconnecting');
    this.#invalidateNativeTransport();
    if (attempt) {
      this.#finishAttempt(attempt);
      attempt.reject(new Error('Discord Gateway reported that the bot left the voice channel'));
    }
    this.#joinRequested = false;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    this.#voiceServerUnavailable = false;
    this.#stopNativeStateMonitor();
    try {
      await this.#nativeWork;
      await this.#backgroundCleanup;
      await this.#closeCurrentNativeSession();
      this.#setConnectionState('disconnected');
    } catch (error) {
      this.#connectionError = asError(error).message;
      this.#setConnectionState('failed');
    }
  }

  async #closeCurrentNativeSession(): Promise<void> {
    await this.#configurationWork;
    const nativeSession = this.#nativeSession;
    if (!nativeSession) return;
    this.#nativeSession = undefined;
    const playback = this.#playback;
    this.#playback = undefined;
    await this.#releaseNativeSession(nativeSession, playback);
  }

  #queueCurrentNativeCleanup(): void {
    const nativeSession = this.#nativeSession;
    if (!nativeSession) return;
    this.#nativeSession = undefined;
    const playback = this.#playback;
    this.#playback = undefined;
    this.#backgroundCleanup = this.#backgroundCleanup
      .then(() => this.#releaseNativeSession(nativeSession, playback))
      .catch((error: unknown) => {
        this.#connectionError ??= asError(error).message;
      });
  }

  async #releaseNativeSession(
    nativeSession: NativeVoiceSessionIds,
    playback: PlaybackSession<Metadata> | undefined,
  ): Promise<void> {
    const errors: Error[] = [];
    try {
      await playback?.releaseInput();
    } catch (error) {
      errors.push(asError(error));
    }
    try {
      await this.#native.disconnectVoice(nativeSession.voiceId);
    } catch (error) {
      errors.push(asError(error));
    }
    try {
      this.#native.releaseVoiceSession(nativeSession.voiceId);
    } catch (error) {
      errors.push(asError(error));
    } finally {
      if (playback) this.#detachPlaybackEvents(playback);
    }
    if (errors.length > 0) {
      throw new AggregateError(errors, 'could not release native Discord voice session');
    }
  }

  async #sendGatewayLeave(): Promise<void> {
    try {
      await this.#gateway.setVoiceChannel(this.guildId, null);
    } catch (error) {
      this.#connectionError ??= asError(error).message;
    }
  }

  #requirePlayback(): PlaybackSession<Metadata> {
    this.#assertNotDisposed();
    if (this.connectionState !== 'connected' || !this.#playback) {
      throw new Error('Discord voice session is not connected');
    }
    return this.#playback;
  }

  #requireExistingPlayback(): PlaybackSession<Metadata> {
    this.#assertNotDisposed();
    if (!this.#playback) throw new Error('Discord voice session has no playback session');
    return this.#playback;
  }

  async #replaceFilterGraph(graph: string | null): Promise<void> {
    this.#assertNotDisposed();
    await this.#serializeConfiguration(async () => {
      if (this.#connectionState === 'disconnecting') {
        throw new Error('Discord voice session is disconnecting');
      }
      const previousGraph = this.#filterGraph;
      if (previousGraph === graph) return;
      if (graph !== null) await this.#native.validateFilterGraph(graph);
      if (this.#playback) {
        if (graph === null) await this.#playback.filters.clear();
        else await this.#playback.filters.set(graph);
      } else {
        this.emit(PlaybackEvent.filterChanged, {
          sessionId: this.sessionId,
          guildId: this.guildId,
          nodeId: null,
          generation: this.#playbackGeneration,
          previousGraph,
          graph,
        });
      }
      this.#filterGraph = graph;
    });
  }

  #serializeConfiguration(operation: () => Promise<void>): Promise<void> {
    const result = this.#configurationWork.then(operation, operation);
    this.#configurationWork = result.catch(() => {});
    return result;
  }

  #assertNotDisposed(): void {
    if (this.#disposed || this.#closing) throw new Error('Discord voice session has been disposed');
  }

  #setConnectionState(state: VoiceConnectionState): void {
    const previousState = normalizeLifecycleVoiceState(this.#connectionState);
    const nextState = normalizeLifecycleVoiceState(state);
    if (previousState === nextState) return;
    this.#connectionState = state;
    this.emit(VoiceEvent.voiceStateChanged, {
      sessionId: this.sessionId,
      guildId: this.guildId,
      nodeId: null,
      previousState,
      state: nextState,
    });
    if (nextState === 'failed') {
      this.emit(VoiceEvent.voiceFailed, {
        sessionId: this.sessionId,
        guildId: this.guildId,
        nodeId: null,
        state: 'failed',
        error: new Error(this.#connectionError ?? 'Discord voice transport failed'),
      });
    }
  }

  #forwardPlaybackEvents(playback: PlaybackSession<Metadata>): void {
    this.#detachPlaybackEvents();
    const playbackEvents = playback as unknown as {
      on<Event extends keyof PlaybackSessionEventMap<Metadata>>(
        name: Event,
        listener: (event: PlaybackSessionEventMap<Metadata>[Event][0]) => void,
      ): unknown;
      off<Event extends keyof PlaybackSessionEventMap<Metadata>>(
        name: Event,
        listener: (event: PlaybackSessionEventMap<Metadata>[Event][0]) => void,
      ): unknown;
    };
    const unsubscribers: Array<() => void> = [];
    const forward = <Event extends keyof PlaybackSessionEventMap<Metadata>>(
      name: Event,
      listener: (event: PlaybackSessionEventMap<Metadata>[Event][0]) => void,
    ): void => {
      playbackEvents.on(name, listener);
      unsubscribers.push(() => playbackEvents.off(name, listener));
    };
    forward(PlaybackEvent.playbackStarted, (event) => {
      this.#playbackGeneration = event.generation;
      this.emit(PlaybackEvent.playbackStarted, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.playbackCompleted, (event) => {
      this.emit(PlaybackEvent.playbackCompleted, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.playbackStopped, (event) => {
      this.emit(PlaybackEvent.playbackStopped, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.playbackFailed, (event) => {
      this.emit(PlaybackEvent.playbackFailed, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.playbackPaused, (event) => {
      this.emit(PlaybackEvent.playbackPaused, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.playbackResumed, (event) => {
      this.emit(PlaybackEvent.playbackResumed, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.playbackSeeked, (event) => {
      this.emit(PlaybackEvent.playbackSeeked, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.filterChanged, (event) => {
      this.emit(PlaybackEvent.filterChanged, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.volumeChanged, (event) => {
      this.emit(PlaybackEvent.volumeChanged, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.prepareRequested, (event) => {
      this.emit(PlaybackEvent.prepareRequested, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.stageReady, (event) => {
      this.emit(PlaybackEvent.stageReady, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.stageFailed, (event) => {
      this.emit(PlaybackEvent.stageFailed, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.stageInvalidated, (event) => {
      this.emit(PlaybackEvent.stageInvalidated, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    forward(PlaybackEvent.transitionCancelled, (event) => {
      this.emit(PlaybackEvent.transitionCancelled, {
        ...event,
        sessionId: this.sessionId,
        guildId: this.guildId,
      });
    });
    this.#playbackSubscriptions = {
      playback,
      detach: () => {
        for (const unsubscribe of unsubscribers.splice(0)) unsubscribe();
      },
    };
  }

  #detachPlaybackEvents(playback?: PlaybackSession<Metadata>): void {
    const subscriptions = this.#playbackSubscriptions;
    if (!subscriptions || (playback && subscriptions.playback !== playback)) return;
    this.#playbackSubscriptions = undefined;
    subscriptions.detach();
  }
}

function validateSnowflake(name: string, value: string): string {
  if (typeof value !== 'string' || !isSnowflake(value)) {
    throw new TypeError(`${name} must be a decimal Discord snowflake string`);
  }
  return value;
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function abortError(message: string): Error {
  const error = new Error(message);
  error.name = 'AbortError';
  return error;
}

function normalizeLifecycleVoiceState(state: VoiceConnectionState): LifecycleVoiceState {
  return state === 'negotiatingDave' ? 'negotiating_dave' : state;
}
