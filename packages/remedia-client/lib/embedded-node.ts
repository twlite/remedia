import { UnsupportedMediaSourceError } from './errors.js';
import { EventEmitter } from 'node:events';
import { RemediaEventEmitter } from './events.js';
import {
  PlaybackSessionEvents as PlaybackEvent,
  VoiceSessionEvents as VoiceEvent,
} from './types.js';
import type { EventEmitter as EventEmitterType } from 'node:events';
import type {
  AudioStreamInfo,
  DiscordSessionOptions,
  MediaSource,
  NodeSnapshot,
  NodeStatistics,
  NodeStatus,
  PlaybackCompletion,
  PlaybackPosition,
  PlaybackSnapshot,
  PlaybackState,
  PlayOptions,
  VoiceConnectionState,
} from './types.js';
import type { GuildBackend, GuildBackendEvents, NodeAdapter } from './adapters.js';

interface EmbeddedPlayback extends EventEmitterType {
  readonly state: PlaybackState;
  readonly position: PlaybackPosition;
  readonly durationMs: number | null;
  readonly error: string | null;
  readonly volume: number;
  readonly filters: {
    readonly graph: string | null;
    set(graph: string): Promise<void>;
    clear(): Promise<void>;
  };
  play(source: MediaSource, options?: PlayOptions<unknown>): Promise<AudioStreamInfo>;
  stageNext(
    source: MediaSource,
    options: import('./types.js').StageNextOptions<unknown>,
  ): Promise<import('./types.js').StageSnapshot>;
  clearStage(): Promise<void>;
  readonly stagedSource: import('./types.js').StageSnapshot | null;
  pause(): Promise<void>;
  resume(): Promise<void>;
  seek(positionMs: number): Promise<void>;
  stop(): Promise<void>;
  wait(): Promise<PlaybackState>;
  setVolume(gain: number): Promise<void>;
}

interface EmbeddedDiscordSession extends EmbeddedPlayback {
  readonly guildId: string;
  readonly connectionState: VoiceConnectionState | 'negotiatingDave';
  readonly connectionError: string | null;
  readonly sessionId: string;
  readonly generation: number;
  move(channelId: string): Promise<void>;
  setSelfDeaf(enabled: boolean): Promise<void>;
  setSelfMute(enabled: boolean): Promise<void>;
  connect(options?: { signal?: AbortSignal; timeoutMs?: number }): Promise<void>;
  disconnect(): Promise<void>;
  dispose(): Promise<void>;
}

interface EmbeddedRemedia {
  probe(source: MediaSource, options?: import('./types.js').ProbeOptions): Promise<AudioStreamInfo>;
  createDiscordSession(options: DiscordSessionOptions): EmbeddedDiscordSession;
  receive(packet: unknown): void;
  close(): Promise<void>;
}

interface EmbeddedRemediaModule {
  Remedia: {
    create(options: { send: import('./types.js').DiscordGatewaySend }): Promise<EmbeddedRemedia>;
  };
}

interface EmbeddedNodeEvents {
  status: { snapshot: NodeSnapshot; error: Error | null };
}

/** Lazy direct-N-API adapter. It does not start or communicate with a local remedia-server. */
export class EmbeddedNodeAdapter implements NodeAdapter {
  readonly type = 'embedded' as const;
  readonly id: string;
  readonly #capacity: number;
  readonly #send: import('./types.js').DiscordGatewaySend;
  readonly #sessions = new Map<string, EmbeddedDiscordSession>();
  readonly #onStatus: (event: EmbeddedNodeEvents['status']) => void;
  #engine: EmbeddedRemedia | undefined;
  #status: NodeStatus = 'connecting';
  #lastError: string | null = null;
  #closed = false;

  constructor(
    options: { id: string; capacity?: number },
    send: import('./types.js').DiscordGatewaySend,
    onStatus: (event: EmbeddedNodeEvents['status']) => void,
  ) {
    this.id = options.id;
    this.#capacity = options.capacity ?? 64;
    if (!Number.isSafeInteger(this.#capacity) || this.#capacity < 1) {
      throw new RangeError(`capacity for node ${this.id} must be a positive safe integer`);
    }
    this.#send = send;
    this.#onStatus = onStatus;
  }

  get status(): NodeStatus {
    return this.#status;
  }

  get snapshot(): NodeSnapshot {
    const active = this.#sessions.size;
    const statistics: NodeStatistics = {
      playersActive: active,
      playersMaximum: this.#capacity,
      connectionsActive: active,
      connectionsMaximum: this.#capacity,
      transportHealth: this.#status === 'healthy' ? 'healthy' : 'unknown',
      processingHealth: this.#status === 'healthy' ? 'healthy' : 'unknown',
      cpu: {},
      memory: {},
    };
    return {
      id: this.id,
      type: this.type,
      status: this.#status,
      nodeId: this.id,
      version: null,
      protocolVersion: null,
      capabilities: {
        crossfade: {
          configured: true,
          available: true,
          durationMs: 5000,
          prepareBeforeMs: 30000,
          curve: 'equalPower',
        },
        // Embedded nodes run in-process and do not talk to configured plugin hosts.
        pluginOperations: { available: false, hosts: [] },
      },
      statistics,
      capacity: this.#capacity,
      assignedGuilds: [...this.#sessions.keys()],
      lastError: this.#lastError,
    };
  }

  /**
   * Inspect disposable media without assigning a guild or creating playback.
   * @param source - Encoded media supported by this backend.
   * @param options - Cancellation and total deadline, defaulting to 15000 ms.
   * @returns Available stream metadata; rejects unavailable nodes, invalid inputs, timeout, or cancellation.
   */
  async probe(
    source: MediaSource,
    options?: import('./types.js').ProbeOptions,
  ): Promise<AudioStreamInfo> {
    if (this.#status !== 'healthy' || !this.#engine)
      throw new Error(`embedded node ${this.id} is unavailable`);
    assertEmbeddedSource(source);
    return this.#engine.probe(source, options);
  }

  async start(): Promise<void> {
    if (this.#closed) return;
    try {
      // Use a variable specifier so TypeScript declarations do not require @remedia/node when a
      // consumer installs the remote-only client package.
      const packageName: string = '@remedia/node';
      const native = (await import(packageName)) as EmbeddedRemediaModule;
      this.#engine = await native.Remedia.create({ send: this.#send });
      this.#status = 'healthy';
      this.#lastError = null;
    } catch (error) {
      this.#status = 'disconnected';
      this.#lastError = 'embedded @remedia/node could not be loaded';
      throw new Error(this.#lastError, { cause: error });
    } finally {
      this.#onStatus({
        snapshot: this.snapshot,
        error: this.#lastError ? new Error(this.#lastError) : null,
      });
    }
  }

  receive(packet: unknown): void {
    this.#engine?.receive(packet);
  }

  createGuildSession(options: DiscordSessionOptions, onDispose: () => void): GuildBackend {
    if (this.#status !== 'healthy' || !this.#engine)
      throw new Error(`embedded node ${this.id} is unavailable`);
    if (this.#sessions.has(options.guildId))
      throw new Error(`guild ${options.guildId} is already assigned to node ${this.id}`);
    const session = this.#engine.createDiscordSession(options);
    this.#sessions.set(options.guildId, session);
    return new EmbeddedGuildBackend(this, session, () => {
      if (this.#sessions.get(options.guildId) === session) this.#sessions.delete(options.guildId);
      onDispose();
    });
  }

  async releaseGuild(guildId: string): Promise<void> {
    const session = this.#sessions.get(guildId);
    this.#sessions.delete(guildId);
    await session?.dispose();
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#status = 'closed';
    try {
      await this.#engine?.close();
    } finally {
      this.#sessions.clear();
      this.#engine = undefined;
      this.#onStatus({ snapshot: this.snapshot, error: null });
    }
  }
}

class EmbeddedGuildBackend extends RemediaEventEmitter<GuildBackendEvents> implements GuildBackend {
  readonly nodeType = 'embedded' as const;
  readonly #node: EmbeddedNodeAdapter;
  readonly #session: EmbeddedDiscordSession;
  readonly #onDispose: () => void;
  readonly #eventUnsubscribers: Array<() => void> = [];
  #generation = 0;
  #snapshot: PlaybackSnapshot = emptySnapshot();
  #disposed = false;

  constructor(node: EmbeddedNodeAdapter, session: EmbeddedDiscordSession, onDispose: () => void) {
    super();
    this.#node = node;
    this.#session = session;
    this.#onDispose = onDispose;
    this.#forwardEvents();
  }

  get nodeId(): string {
    return this.#node.id;
  }
  get state(): PlaybackState {
    return this.#session.state;
  }
  get position(): PlaybackPosition {
    return this.#session.position;
  }
  get durationMs(): number | null {
    return this.#session.durationMs;
  }
  get error(): string | null {
    return this.#session.error;
  }
  get volume(): number {
    return this.#session.volume;
  }
  get filterGraph(): string | null {
    return this.#session.filters.graph;
  }
  get connectionState(): VoiceConnectionState {
    return normalizeVoiceState(this.#session.connectionState);
  }

  get snapshot(): PlaybackSnapshot {
    return {
      ...this.#snapshot,
      state: this.state,
      generation: this.#generation,
      position: this.position,
      durationMs: this.durationMs,
      filterGraph: this.filterGraph,
      volume: this.volume,
      staged: this.#session.stagedSource,
      lastError: this.error ? { code: 'playback_failed', message: this.error } : null,
    };
  }

  move(channelId: string): Promise<void> {
    return this.#session.move(channelId);
  }
  setSelfDeaf(enabled: boolean): Promise<void> {
    return this.#session.setSelfDeaf(enabled);
  }
  setSelfMute(enabled: boolean): Promise<void> {
    return this.#session.setSelfMute(enabled);
  }

  async connect(options?: { signal?: AbortSignal; timeoutMs?: number }): Promise<void> {
    this.#assertOpen();
    await this.#session.connect(options);
  }

  async disconnect(): Promise<void> {
    if (!this.#disposed) await this.#session.disconnect();
  }

  async dispose(): Promise<void> {
    if (this.#disposed) return;
    this.#disposed = true;
    try {
      await this.#node.releaseGuild(this.#session.guildId);
    } finally {
      for (const unsubscribe of this.#eventUnsubscribers.splice(0)) unsubscribe();
      this.removeAllListeners();
      this.#onDispose();
    }
  }

  async play(
    source: MediaSource,
    options: PlayOptions<unknown> = {},
  ): Promise<{ info: AudioStreamInfo; generation: number }> {
    this.#assertOpen();
    assertEmbeddedSource(source);
    const info = await this.#session.play(source, options);
    this.#generation = this.#session.generation;
    this.#snapshot = {
      ...this.snapshot,
      source: info,
      state: this.state,
      generation: this.#generation,
    };
    return { info, generation: this.#generation };
  }

  stageNext(
    source: MediaSource,
    options: import('./types.js').StageNextOptions<unknown>,
  ): Promise<import('./types.js').StageSnapshot> {
    this.#assertOpen();
    assertEmbeddedSource(source);
    return this.#session.stageNext(source, options);
  }

  clearStage(): Promise<void> {
    this.#assertOpen();
    return this.#session.clearStage();
  }

  async stagedSource(): Promise<import('./types.js').StageSnapshot | null> {
    this.#assertOpen();
    return this.#session.stagedSource;
  }

  pause(): Promise<void> {
    this.#assertOpen();
    return this.#session.pause();
  }
  resume(): Promise<void> {
    this.#assertOpen();
    return this.#session.resume();
  }
  seek(positionMs: number): Promise<void> {
    this.#assertOpen();
    return this.#session.seek(positionMs);
  }
  stop(): Promise<void> {
    this.#assertOpen();
    return this.#session.stop();
  }

  async wait(): Promise<PlaybackCompletion> {
    this.#assertOpen();
    const generation = this.#generation;
    const state = await this.#session.wait();
    return {
      generation,
      state: state === 'failed' || state === 'stopped' ? state : 'completed',
    };
  }

  async setFilterGraph(graph: string | null): Promise<void> {
    this.#assertOpen();
    if (graph === null) await this.#session.filters.clear();
    else await this.#session.filters.set(graph);
    this.#snapshot = { ...this.#snapshot, filterGraph: graph };
  }

  async setVolume(gain: number): Promise<void> {
    this.#assertOpen();
    await this.#session.setVolume(gain);
    this.#snapshot = { ...this.#snapshot, volume: gain };
  }

  async refresh(): Promise<PlaybackSnapshot> {
    this.#assertOpen();
    this.#snapshot = this.snapshot;
    return this.#snapshot;
  }

  markSessionLost(reason: string): void {
    this.#snapshot = {
      ...this.snapshot,
      state: 'failed',
      lastError: { code: 'unknown_session', message: reason },
    };
  }

  #assertOpen(): void {
    if (this.#disposed) throw new Error('Discord session has been disposed');
  }

  #forwardEvents(): void {
    const source = this.#session as unknown as EventEmitter;
    const add = (name: keyof GuildBackendEvents, payload: Record<string, unknown>): void => {
      const generation =
        typeof payload.generation === 'number' ? payload.generation : this.#generation;
      this.#generation =
        name === PlaybackEvent.transitionCancelled
          ? generation
          : Math.max(this.#generation, generation);
      const normalized = {
        ...payload,
        sessionId: this.#session.sessionId,
        guildId: this.#session.guildId,
        nodeId: this.#node.id,
      };
      (this as unknown as EventEmitter).emit(name, normalized);
    };
    const subscribe = (name: keyof GuildBackendEvents): void => {
      const listener = (event: Record<string, unknown>): void => add(name, event);
      source.on(name, listener);
      this.#eventUnsubscribers.push(() => source.off(name, listener));
    };
    subscribe(PlaybackEvent.playbackStarted);
    subscribe(PlaybackEvent.playbackCompleted);
    subscribe(PlaybackEvent.playbackStopped);
    subscribe(PlaybackEvent.playbackFailed);
    subscribe(PlaybackEvent.playbackPaused);
    subscribe(PlaybackEvent.playbackResumed);
    subscribe(PlaybackEvent.playbackSeeked);
    subscribe(PlaybackEvent.prepareRequested);
    subscribe(PlaybackEvent.stageReady);
    subscribe(PlaybackEvent.stageFailed);
    subscribe(PlaybackEvent.stageInvalidated);
    subscribe(PlaybackEvent.transitionCancelled);
    subscribe(PlaybackEvent.filterChanged);
    subscribe(PlaybackEvent.volumeChanged);
    subscribe(VoiceEvent.voiceStateChanged);
    subscribe(VoiceEvent.voiceFailed);
  }
}

function normalizeVoiceState(
  value: VoiceConnectionState | 'negotiatingDave',
): VoiceConnectionState {
  return value === 'negotiatingDave' ? 'negotiating_dave' : value;
}

function emptySnapshot(): PlaybackSnapshot {
  return {
    state: 'idle',
    generation: 0,
    processingMode: null,
    position: { mediaMs: null, mediaPositionAccuracy: null, submittedMs: 0, playedMs: null },
    durationMs: null,
    source: null,
    filterGraph: null,
    volume: 1,
    lastError: null,
  };
}

/** Embedded engines execute no plugin operations; reject them instead of falling back. */
function assertEmbeddedSource(source: MediaSource): void {
  if ('hostId' in source && 'pluginId' in source && 'operation' in source)
    throw new UnsupportedMediaSourceError(
      'embedded Remedia nodes do not support plugin operation sources; use a remote node',
    );
}
