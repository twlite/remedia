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

export interface DiscordSessionOptions {
  /** Discord server snowflake as a decimal string. */
  guildId: string;
  /** Voice channel snowflake as a decimal string. */
  channelId: string;
  /** Bot user snowflake as a decimal string. */
  userId: string;
}

export interface DiscordConnectOptions {
  /** Abort the Gateway request and any pending connection attempt. */
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
}

/**
 * One Discord voice session. The caller supplies a Gateway send adapter to `Remedia.create()` and
 * forwards raw dispatch packets to `Remedia.receive()`; this session collects the matching voice
 * credentials and opens the native transport after both dispatches arrive.
 */
export class DiscordSession<Metadata = unknown> extends RemediaEventEmitter<
  SessionLifecycleEvents<Metadata>
> {
  readonly sessionId = randomUUID();
  readonly guildId: string;
  readonly channelId: string;
  readonly userId: string;

  readonly #native: NativeRemedia;
  readonly #gateway: DiscordGatewayAdapter;
  readonly #onDispose: () => void;
  readonly filters: FilterControls;
  readonly hooks = new PlaybackHooks<Metadata>();
  #playback: PlaybackSession<Metadata> | undefined;
  #playbackSubscriptions: { playback: PlaybackSession<Metadata>; detach: () => void } | undefined;
  #nativeSession: NativeVoiceSessionIds | undefined;
  #attempt: ConnectAttempt | undefined;
  #voiceState: VoiceStateCredentials | undefined;
  #voiceServer: VoiceServerCredentials | undefined;
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
  #configurationWork: Promise<void> = Promise.resolve();

  /** @internal Constructed by `Remedia.createDiscordSession()`. */
  constructor(
    native: NativeRemedia,
    gateway: DiscordGatewayAdapter,
    options: DiscordSessionOptions,
    onDispose: () => void,
  ) {
    super();
    this.guildId = validateSnowflake('guildId', options.guildId);
    this.channelId = validateSnowflake('channelId', options.channelId);
    this.userId = validateSnowflake('userId', options.userId);
    this.#native = native;
    this.#gateway = gateway;
    this.#onDispose = onDispose;
    this.filters = new FilterControls(
      () => this.#filterGraph,
      (graph) => this.#replaceFilterGraph(graph),
      () => this.#replaceFilterGraph(null),
    );
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

  get state(): PlaybackState {
    if (this.#playback) return this.#playback.state;
    return this.connectionState === 'failed' ? 'failed' : 'idle';
  }

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

  get durationMs(): number | null {
    return this.#playback?.durationMs ?? null;
  }

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

  /** Set master gain independently of this session's configured filtergraph. */
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

    const attempt = this.#createAttempt(timeoutMs, options.signal);
    this.#attempt = attempt;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
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
   */
  play(source: MediaSource, options: PlayOptions<Metadata> = {}): Promise<AudioStreamInfo> {
    return this.#requirePlayback()
      .play(source, options)
      .then((info) => {
        this.#playbackGeneration = this.#playback?.currentGeneration ?? this.#playbackGeneration;
        return info;
      });
  }

  pause(): Promise<void> {
    return this.#requirePlayback().pause();
  }

  resume(): Promise<void> {
    return this.#requirePlayback().resume();
  }

  seek(positionMs: number): Promise<void> {
    return this.#requirePlayback().seek(positionMs);
  }

  stop(): Promise<void> {
    return this.#requirePlayback().stop();
  }

  wait(): Promise<PlaybackState> {
    return this.#requirePlayback().wait();
  }

  /**
   * Leave the Discord channel through Gateway OP 4 and close the native transport. This does not
   * dispose the wrapper, so `connect()` can be called again with fresh Gateway dispatches.
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

  /** Disconnect and unregister this session. */
  async dispose(): Promise<void> {
    if (this.#disposed) return;
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

  /** @internal Routes a parsed raw Gateway dispatch to this guild's active voice lifecycle. */
  receiveGatewayDispatch(event: string, data: Record<string, unknown>, sequence?: number): void {
    if (this.#disposed || data.guild_id !== this.guildId) return;
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
      if (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting') {
        this.#handleGatewayDisconnect();
      }
      // A null channel can be a delayed state from a previous leave while a new join is pending.
      return;
    }
    if (data.channel_id !== this.channelId || typeof data.session_id !== 'string') return;
    if (data.session_id.length === 0) return;

    if (this.#voiceState?.sessionId === data.session_id) return;
    const refreshRequired =
      !this.#attempt &&
      (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting');
    this.#voiceState = { sessionId: data.session_id };
    if (refreshRequired) {
      this.#beginCredentialRefresh();
      return;
    }
    this.#credentialsChanged();
  }

  #receiveVoiceServer(data: Record<string, unknown>): void {
    if (!this.#joinRequested || this.#connectionState === 'disconnecting') return;
    if (data.endpoint === null) {
      this.#failGateway(new Error('Discord did not provide a voice server endpoint'));
      return;
    }
    if (typeof data.token !== 'string' || typeof data.endpoint !== 'string') return;
    if (data.token.length === 0 || data.endpoint.length === 0) return;

    if (
      !this.#attempt &&
      (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting')
    ) {
      this.#beginCredentialRefresh();
      return;
    }

    if (this.#voiceServer?.token === data.token && this.#voiceServer.endpoint === data.endpoint) {
      return;
    }
    this.#voiceServer = { token: data.token, endpoint: data.endpoint };
    this.#credentialsChanged();
  }

  #credentialsChanged(): void {
    const attempt = this.#attempt;
    if (!attempt || !this.#voiceState || !this.#voiceServer || !attempt.requestSent) return;
    this.#tryStartNativeConnection(attempt);
  }

  #createAttempt(timeoutMs: number, signal: AbortSignal | undefined): ConnectAttempt {
    let resolve!: () => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<void>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    this.#generation += 1;
    let attempt!: ConnectAttempt;
    attempt = {
      generation: this.#generation,
      sequenceFloor: this.#gateway.lastSequence,
      promise,
      resolve,
      reject,
      timer: setTimeout(() => {
        this.#failAttempt(
          attempt,
          new Error(`Discord voice connection timed out after ${timeoutMs} ms`),
          true,
        );
      }, timeoutMs),
      signal,
      abortListener: undefined,
      requestSent: false,
      settled: false,
    };
    return attempt;
  }

  #requestGatewayJoin(attempt: ConnectAttempt): void {
    let sendPromise: void | Promise<void>;
    try {
      sendPromise = this.#gateway.setVoiceChannel(this.guildId, this.channelId);
    } catch (error) {
      this.#failAttempt(attempt, asError(error), true);
      return;
    }
    void Promise.resolve(sendPromise)
      .then(() => {
        if (this.#attempt !== attempt) return;
        attempt.requestSent = true;
        this.#tryStartNativeConnection(attempt);
      })
      .catch((error: unknown) => {
        this.#failAttempt(attempt, asError(error), true);
      });
  }

  #beginCredentialRefresh(): void {
    if (this.#attempt || this.#disposed) return;
    const attempt = this.#createAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined);
    // This internal attempt may have no direct caller waiting on its Promise.
    void attempt.promise.catch(() => {});
    this.#attempt = attempt;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    this.#setConnectionState('reconnecting');
    this.#connectionError = undefined;
    this.#joinRequested = true;
    this.#requestGatewayJoin(attempt);
  }

  #tryStartNativeConnection(attempt: ConnectAttempt): void {
    if (
      this.#attempt !== attempt ||
      !attempt.requestSent ||
      !this.#voiceState ||
      !this.#voiceServer
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
    this.#setConnectionState(this.#nativeSession ? 'reconnecting' : 'negotiatingDave');

    const work = this.#nativeWork.then(async () => {
      if (!this.#isCurrent(generation, attempt)) return;
      await this.#backgroundCleanup;
      if (!this.#isCurrent(generation, attempt)) return;
      await this.#closeCurrentNativeSession();
      if (!this.#isCurrent(generation, attempt)) return;

      const options: NativeVoiceOptions = {
        guildId: this.guildId,
        channelId: this.channelId,
        userId: this.userId,
        sessionId: credentials.sessionId,
        token: credentials.token,
        endpoint: credentials.endpoint,
      };
      const nativeSession = await this.#native.connectVoice(options);
      if (!this.#isCurrent(generation, attempt)) {
        await this.#releaseNativeSession(nativeSession, undefined);
        return;
      }

      this.#nativeSession = nativeSession;
      const playback = new PlaybackSession<Metadata>(
        this.#native,
        nativeSession.playbackId,
        undefined,
        this.guildId,
        this.hooks,
      );
      this.#playback = playback;
      await this.#serializeConfiguration(async () => {
        if (this.#playback !== playback) return;
        if (this.#filterGraph !== null) await playback.filters.set(this.#filterGraph);
        if (this.#volume !== 1) await playback.setVolume(this.#volume);
      });
      if (!this.#isCurrent(generation, attempt)) {
        await this.#closeCurrentNativeSession();
        return;
      }
      this.#connectionError = undefined;
      this.#setConnectionState('connected');
      this.#forwardPlaybackEvents(playback);
      this.#playbackGeneration = playback.currentGeneration;
      this.#voiceServer = undefined;
      if (attempt && this.#attempt === attempt) {
        this.#finishAttempt(attempt);
        attempt.resolve();
      }
    });

    this.#nativeWork = work.catch((error: unknown) => {
      this.#nativeConnectionFailed(asError(error), generation, attempt);
    });
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
    this.#connectionError = error.message;
    this.#setConnectionState('failed');
    const pending = this.#attempt;
    if (pending) {
      this.#finishAttempt(pending);
      pending.reject(error);
    }
    const shouldLeaveGateway = this.#joinRequested;
    this.#joinRequested = false;
    this.#generation += 1;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    this.#queueCurrentNativeCleanup();
    if (shouldLeaveGateway) void this.#sendGatewayLeave();
  }

  #failGateway(error: Error): void {
    const attempt = this.#attempt;
    if (attempt) {
      this.#failAttempt(attempt, error, true);
      return;
    }
    if (this.#connectionState === 'connected' || this.#connectionState === 'reconnecting') {
      this.#nativeConnectionFailed(error, this.#generation, undefined);
    }
  }

  #failAttempt(attempt: ConnectAttempt, error: Error, leaveGateway: boolean): void {
    if (this.#attempt !== attempt) return;
    this.#generation += 1;
    this.#finishAttempt(attempt);
    this.#connectionError = error.name === 'AbortError' ? undefined : error.message;
    this.#setConnectionState(error.name === 'AbortError' ? 'disconnected' : 'failed');
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
    this.#queueCurrentNativeCleanup();
    const shouldLeaveGateway = leaveGateway && this.#joinRequested;
    this.#joinRequested = false;
    attempt.reject(error);
    if (shouldLeaveGateway) void this.#sendGatewayLeave();
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
    if (attempt) {
      this.#finishAttempt(attempt);
      attempt.reject(new Error('Discord Gateway reported that the bot left the voice channel'));
    }
    this.#joinRequested = false;
    this.#voiceState = undefined;
    this.#voiceServer = undefined;
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
    if (this.#disposed) throw new Error('Discord voice session has been disposed');
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
