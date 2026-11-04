import { randomUUID } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { RemediaEventEmitter } from './events.js';
import {
  PlaybackSessionEvents as PlaybackEvent,
  VoiceSessionEvents as VoiceEvent,
} from './types.js';
import { SessionLostError, asError } from './errors.js';
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
  retryCount: number;
}

export interface DiscordSessionRuntime {
  sendVoicePayload(guildId: string, channelId: string | null): void | Promise<void>;
  gatewaySequence(): number | undefined;
  resolveBackend(options: DiscordSessionOptions, onDispose: () => void): Promise<GuildBackend>;
  releaseSession(session: { readonly guildId: string }): void;
}

const DEFAULT_CONNECT_TIMEOUT_MS = 45_000;

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

/** A Discord voice player with a node-sticky backend, whether embedded or remote. */
export class DiscordSession<Metadata = unknown> extends RemediaEventEmitter<
  DiscordSessionEvents<Metadata>
> {
  readonly sessionId = randomUUID();
  readonly guildId: string;
  readonly channelId: string;
  readonly userId: string;
  readonly #runtime: DiscordSessionRuntime;
  readonly #options: DiscordSessionOptions;
  readonly filters: FilterControls;
  readonly hooks = new PlaybackHooks<Metadata>();
  readonly #metadataByGeneration = new Map<number, Metadata | undefined>();
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

  constructor(runtime: DiscordSessionRuntime, options: DiscordSessionOptions) {
    super();
    this.#runtime = runtime;
    this.#options = options;
    this.guildId = validateSnowflake('guildId', options.guildId);
    this.channelId = validateSnowflake('channelId', options.channelId);
    this.userId = validateSnowflake('userId', options.userId);
    this.filters = new FilterControls(
      () => this.#filterGraph,
      (graph) => this.#replaceFilterGraph(graph),
      () => this.#replaceFilterGraph(null),
    );
  }

  get nodeId(): string | null {
    return this.#backend?.nodeId ?? null;
  }

  get nodeType(): 'embedded' | 'remote' | null {
    return this.#backend?.nodeType ?? null;
  }

  get connectionState(): VoiceConnectionState {
    if (this.#disposed) return 'disconnected';
    if (
      this.#connectionState === 'connecting' ||
      this.#connectionState === 'negotiating_dave' ||
      this.#connectionState === 'disconnecting' ||
      this.#connectionState === 'failed'
    )
      return this.#connectionState;
    return this.#backend?.connectionState ?? this.#connectionState;
  }

  get connectionError(): string | null {
    return this.#connectionError ?? this.#backend?.error ?? null;
  }

  get state(): PlaybackState {
    return this.#backend?.state ?? (this.connectionState === 'failed' ? 'failed' : 'idle');
  }

  get position(): PlaybackPosition {
    return this.#backend?.position ?? emptyPosition();
  }

  get durationMs(): number | null {
    return this.#backend?.durationMs ?? null;
  }

  get error(): string | null {
    return this.#backend?.error ?? this.#connectionError;
  }

  get volume(): number {
    return this.#backend?.volume ?? this.#volume;
  }

  get snapshot(): PlaybackSnapshot {
    return this.#backend?.snapshot ?? emptySnapshot(this.#filterGraph, this.#volume);
  }

  get assignmentStatus(): 'unassigned' | 'assigned' | 'lost' {
    if (!this.#backend) return 'unassigned';
    return this.#backend.state === 'failed' && this.#backend.error?.includes('session')
      ? 'lost'
      : 'assigned';
  }

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

  play(source: MediaSource, options: PlayOptions<Metadata> = {}): Promise<AudioStreamInfo> {
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
      const playRequest = backend.play(source, {
        ...(options.customMetadata === undefined ? {} : { customMetadata: options.customMetadata }),
        signal: controller.signal,
      });
      backendStarted = true;
      const result = await playRequest;
      this.#playbackGeneration = result.generation;
      if (abortBinding) abortBinding.generation = result.generation;
      this.#metadataByGeneration.set(result.generation, options.customMetadata);
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

  pause(): Promise<void> {
    return this.#requireExistingBackend().pause();
  }
  resume(): Promise<void> {
    return this.#requireExistingBackend().resume();
  }
  seek(positionMs: number): Promise<void> {
    if (!Number.isFinite(positionMs) || positionMs < 0)
      return Promise.reject(new RangeError('positionMs must be non-negative'));
    return this.#requireExistingBackend().seek(positionMs);
  }
  stop(): Promise<void> {
    return this.#stopPlayback();
  }

  async #stopPlayback(): Promise<void> {
    await this.#requireExistingBackend().stop();
    this.#clearPlayAbortBinding();
  }

  async wait(): Promise<PlaybackState> {
    this.#assertOpen();
    if (this.#playPromise) await this.#playPromise;
    const completion = await this.#requireExistingBackend().wait();
    if (completion.generation !== this.#playbackGeneration) return 'stopped';
    return completion.state;
  }

  async refresh(): Promise<PlaybackSnapshot> {
    this.#assertOpen();
    if (!this.#backend) return this.snapshot;
    return this.#backend.refresh();
  }

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

  disconnect(): Promise<void> {
    if (this.#disposed) return Promise.resolve();
    return this.#disconnect(true);
  }

  async dispose(): Promise<void> {
    if (this.#disposed) return;
    const abortBinding = this.#playAbortBinding;
    this.#clearPlayAbortBinding(abortBinding);
    (abortBinding?.controller ?? this.#playController)?.abort(
      new Error('Discord session disposed'),
    );
    try {
      await this.#disconnect(true);
    } finally {
      this.#disposed = true;
      try {
        this.#detachBackend();
        await this.#backend?.dispose();
      } finally {
        this.hooks.clear();
        this.#metadataByGeneration.clear();
        this.#earlyPlaybackEvents.clear();
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

  /** @internal */
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

  /** @internal */
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
      await this.#applyConfiguration(backend);
      if (backend.nodeType === 'embedded') {
        await backend.connect(options);
        if (!this.#isCurrent(attempt)) {
          await backend.disconnect();
          return;
        }
        this.#setConnectionState('connected');
        this.#finishAttempt(attempt);
        attempt.resolve();
      } else {
        backend.validateIdentity?.(this.userId);
        this.#setConnectionState(attempt.internal ? 'reconnecting' : 'connecting');
        this.#joinRequested = true;
        await this.#runtime.sendVoicePayload(this.guildId, this.channelId);
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
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, this.channelId))
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
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, this.channelId))
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
    void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, this.channelId))
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
          sessionId: state.sessionId,
          token: server.token,
          endpoint: server.endpoint,
        };
        const credentialRevision = attempt.credentialRevision;
        try {
          await attempt.voiceSuspension;
          if (credentialRevision !== attempt.credentialRevision) continue;
          await backend.connectWithCredentials?.(credentials, transportGeneration);
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
    this.#finishAttempt(attempt);
    this.#connectionError = error.name === 'AbortError' ? null : error.message;
    this.#setConnectionState(error.name === 'AbortError' ? 'disconnected' : 'failed');
    if (error.name !== 'AbortError') this.#emitVoiceFailure(error);
    if (attempt.internal && error.name !== 'AbortError') {
      const transportGeneration = this.#nextTransportGeneration();
      void this.#backend?.failVoiceRecovery?.(transportGeneration).catch(() => undefined);
    }
    const shouldLeave = leaveGateway && this.#joinRequested;
    this.#joinRequested = false;
    attempt.reject(error);
    if (shouldLeave)
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, null)).catch(
        () => undefined,
      );
  }

  #retryGatewayCredentials(attempt: ConnectAttempt, _error: Error): boolean {
    if (
      !attempt.internal ||
      !this.#isCurrent(attempt) ||
      attempt.retryCount >= 2 ||
      this.#disposed
    ) {
      return false;
    }
    attempt.retryCount += 1;
    attempt.requestSent = false;
    attempt.sequenceFloor = this.#runtime.gatewaySequence();
    attempt.voiceState ??= this.#voiceSessionId ? { sessionId: this.#voiceSessionId } : undefined;
    attempt.voiceServer = undefined;
    attempt.credentialRevision += 1;
    this.#suspendRemoteVoice(attempt);
    this.#setConnectionState('reconnecting');
    void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, this.channelId))
      .then(() => {
        if (!this.#isCurrent(attempt)) return;
        attempt.requestSent = true;
        this.#tryStartRemoteConnection(attempt);
      })
      .catch((error: unknown) => {
        this.#failAttempt(attempt, asError(error), true);
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
    if (!this.#backend) {
      let backend: GuildBackend | undefined;
      backend = await this.#runtime.resolveBackend(this.#options, () => {
        if (this.#backend === backend) this.#backend = undefined;
        this.#runtime.releaseSession(this);
      });
      this.#backend = backend;
      await this.#applyConfiguration(backend);
      this.#bindBackend(backend);
    }
    return this.#backend;
  }

  async #replaceFilterGraph(graph: string | null): Promise<void> {
    this.#assertOpen();
    await this.#serializeConfiguration(async () => {
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
    bindPlayback(PlaybackEvent.filterChanged);
    bindPlayback(PlaybackEvent.volumeChanged);

    const onVoiceState = (event: VoiceLifecycleEventMap['voiceStateChanged'][0]): void => {
      const state = event.state;
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

    if (event === PlaybackEvent.playbackStarted) this.#startedGenerations.add(generation);
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
    if (this.#disposed) throw new Error('Discord session has been disposed');
    if (this.#sessionLossReason) {
      throw new SessionLostError(
        this.#backend?.nodeId ?? 'unknown',
        [this.guildId],
        this.#sessionLossReason,
      );
    }
  }
}

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
