import { SessionLostError, asError } from './errors.js';
import { FilterControls, validateVolume } from './filters.js';
import type { GuildBackend } from './adapters.js';
import type {
  AudioStreamInfo,
  DiscordConnectOptions,
  DiscordGatewayPacket,
  DiscordSessionOptions,
  MediaSource,
  PlaybackPosition,
  PlaybackSnapshot,
  PlaybackState,
  VoiceConnectionState,
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
  internal: boolean;
}

export interface DiscordSessionRuntime {
  sendVoicePayload(guildId: string, channelId: string | null): void | Promise<void>;
  gatewaySequence(): number | undefined;
  resolveBackend(options: DiscordSessionOptions, onDispose: () => void): Promise<GuildBackend>;
  releaseSession(session: DiscordSession): void;
}

const DEFAULT_CONNECT_TIMEOUT_MS = 45_000;

/** A Discord voice player with a node-sticky backend, whether embedded or remote. */
export class DiscordSession {
  readonly guildId: string;
  readonly channelId: string;
  readonly userId: string;
  readonly #runtime: DiscordSessionRuntime;
  readonly #options: DiscordSessionOptions;
  readonly filters: FilterControls;
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
  #configurationWork: Promise<void> = Promise.resolve();
  #connectionGeneration = 0;
  #playbackGeneration = 0;
  #playPromise: Promise<AudioStreamInfo> | undefined;
  #sessionLossReason: string | undefined;

  constructor(runtime: DiscordSessionRuntime, options: DiscordSessionOptions) {
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
      this.#connectionState === 'disconnecting'
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
    this.#connectionState = 'connecting';
    this.#connectionError = null;

    void this.#connect(attempt, options);
    return attempt.promise;
  }

  play(source: MediaSource): Promise<AudioStreamInfo> {
    this.#assertOpen();
    if (this.connectionState !== 'connected' || !this.#backend) {
      return Promise.reject(new Error('Discord voice session is not connected'));
    }
    const work = this.#backend.play(source).then(({ info, generation }) => {
      this.#playbackGeneration = generation;
      return info;
    });
    this.#playPromise = work;
    return work;
  }

  pause(): Promise<void> {
    return this.#requireBackend().pause();
  }
  resume(): Promise<void> {
    return this.#requireBackend().resume();
  }
  seek(positionMs: number): Promise<void> {
    if (!Number.isFinite(positionMs) || positionMs < 0)
      return Promise.reject(new RangeError('positionMs must be non-negative'));
    return this.#requireBackend().seek(positionMs);
  }
  stop(): Promise<void> {
    return this.#requireBackend().stop();
  }

  async wait(): Promise<PlaybackState> {
    this.#assertOpen();
    if (this.#playPromise) await this.#playPromise;
    const completion = await this.#requireBackend().wait();
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
      await this.#backend?.setVolume(gain);
      this.#volume = gain;
    });
  }

  disconnect(): Promise<void> {
    if (this.#disposed) return Promise.resolve();
    return this.#disconnect(true);
  }

  async dispose(): Promise<void> {
    if (this.#disposed) return;
    try {
      await this.#disconnect(true);
    } finally {
      this.#disposed = true;
      try {
        await this.#backend?.dispose();
      } finally {
        this.#backend = undefined;
        this.#runtime.releaseSession(this);
      }
    }
  }

  /** @internal */
  receiveGatewayDispatch(event: string, data: Record<string, unknown>, sequence?: number): void {
    if (this.#disposed || this.#backend?.nodeType !== 'remote' || data.guild_id !== this.guildId)
      return;
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
    this.#backend?.markSessionLost(reason);
    this.#connectionState = 'failed';
    this.#connectionError = reason;
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
        this.#connectionState = 'connected';
        this.#finishAttempt(attempt);
        attempt.resolve();
      } else {
        backend.validateIdentity?.(this.userId);
        this.#connectionState = attempt.internal ? 'reconnecting' : 'connecting';
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
    if (
      !this.#attempt &&
      this.#connectionState === 'connected' &&
      data.session_id === this.#voiceSessionId
    )
      return;
    const refresh = !this.#attempt && this.#connectionState === 'connected';
    const attempt =
      this.#attempt ??
      (refresh ? this.#newAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true) : undefined);
    if (!attempt) return;
    if (refresh) {
      this.#attempt = attempt;
      attempt.voiceServer = undefined;
      this.#connectionState = 'reconnecting';
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, this.channelId))
        .then(() => {
          if (this.#attempt === attempt) attempt.requestSent = true;
          this.#tryStartRemoteConnection(attempt);
        })
        .catch((error: unknown) => this.#failAttempt(attempt, asError(error), true));
    }
    attempt.voiceState = { sessionId: data.session_id };
    this.#tryStartRemoteConnection(attempt);
  }

  #receiveVoiceServer(data: Record<string, unknown>): void {
    if (!this.#joinRequested) return;
    if (data.endpoint === null) {
      const attempt = this.#attempt;
      if (attempt)
        this.#failAttempt(
          attempt,
          new Error('Discord did not provide a voice server endpoint'),
          true,
        );
      return;
    }
    if (
      typeof data.token !== 'string' ||
      !data.token ||
      typeof data.endpoint !== 'string' ||
      !data.endpoint
    )
      return;
    if (
      !this.#attempt &&
      this.#connectionState === 'connected' &&
      data.token === this.#voiceServer?.token &&
      data.endpoint === this.#voiceServer.endpoint
    )
      return;
    const refresh = !this.#attempt && this.#connectionState === 'connected';
    const attempt =
      this.#attempt ??
      (refresh ? this.#newAttempt(DEFAULT_CONNECT_TIMEOUT_MS, undefined, true) : undefined);
    if (!attempt) return;
    if (refresh) {
      this.#attempt = attempt;
      attempt.voiceState = undefined;
      this.#connectionState = 'reconnecting';
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, this.channelId))
        .then(() => {
          if (this.#attempt === attempt) attempt.requestSent = true;
          this.#tryStartRemoteConnection(attempt);
        })
        .catch((error: unknown) => this.#failAttempt(attempt, asError(error), true));
    }
    attempt.voiceServer = { token: data.token, endpoint: data.endpoint };
    this.#tryStartRemoteConnection(attempt);
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
      const credentials = {
        sessionId: attempt.voiceState!.sessionId,
        token: attempt.voiceServer!.token,
        endpoint: attempt.voiceServer!.endpoint,
      };
      const backend = await this.#ensureBackend();
      if (attempt.internal) await backend.disconnect();
      await backend.connectWithCredentials?.(credentials);
      if (!this.#isCurrent(attempt)) {
        await backend.disconnect();
        return;
      }
      this.#joinRequested = true;
      this.#voiceSessionId = credentials.sessionId;
      this.#voiceServer = { token: credentials.token, endpoint: credentials.endpoint };
      this.#connectionError = null;
      this.#connectionState = 'connected';
      this.#finishAttempt(attempt);
      attempt.resolve();
    })().catch((error: unknown) => this.#failAttempt(attempt, asError(error), true));
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
    Object.assign(attempt, {
      generation: this.#connectionGeneration + 1,
      sequenceFloor: this.#runtime.gatewaySequence(),
      resolve,
      reject,
      promise,
      timer: setTimeout(
        () =>
          this.#failAttempt(
            attempt,
            new Error(`Discord voice connection timed out after ${timeoutMs} ms`),
            true,
          ),
        timeoutMs,
      ),
      signal,
      abortListener: undefined,
      requestSent: false,
      settled: false,
      starting: false,
      voiceState: undefined,
      voiceServer: undefined,
      internal,
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
    this.#joinRequested = false;
    this.#connectionState = 'disconnecting';
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
      await this.#backend?.disconnect();
    } catch (error) {
      errors.push(asError(error));
    }
    this.#voiceSessionId = undefined;
    this.#voiceServer = undefined;
    this.#connectionState = errors.length ? 'failed' : 'disconnected';
    if (errors.length) {
      this.#connectionError = errors.map((error) => error.message).join('; ');
      throw new AggregateError(errors, 'Discord voice disconnection failed');
    }
  }

  #handleExternalDisconnect(): void {
    this.#joinRequested = false;
    this.#voiceSessionId = undefined;
    this.#voiceServer = undefined;
    const attempt = this.#attempt;
    if (attempt) {
      this.#finishAttempt(attempt);
      attempt.reject(new Error('Discord Gateway reported that the bot left the voice channel'));
    }
    this.#connectionState = 'disconnected';
    void this.#backend?.disconnect().catch((error: unknown) => {
      this.#connectionError = asError(error).message;
      this.#connectionState = 'failed';
    });
  }

  #failAttempt(attempt: ConnectAttempt, error: Error, leaveGateway: boolean): void {
    if (this.#attempt !== attempt) return;
    this.#finishAttempt(attempt);
    this.#connectionState = error.name === 'AbortError' ? 'disconnected' : 'failed';
    this.#connectionError = error.name === 'AbortError' ? null : error.message;
    const shouldLeave = leaveGateway && this.#joinRequested;
    this.#joinRequested = false;
    attempt.reject(error);
    if (shouldLeave)
      void Promise.resolve(this.#runtime.sendVoicePayload(this.guildId, null)).catch(
        () => undefined,
      );
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
    }
    return this.#backend;
  }

  async #replaceFilterGraph(graph: string | null): Promise<void> {
    this.#assertOpen();
    await this.#serializeConfiguration(async () => {
      await this.#backend?.setFilterGraph(graph);
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

  #requireBackend(): GuildBackend {
    this.#assertOpen();
    if (this.#connectionState !== 'connected' || !this.#backend)
      throw new Error('Discord voice session is not connected');
    return this.#backend;
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
