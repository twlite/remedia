import { EventEmitter } from 'node:events';
import { WebSocket } from 'ws';
import {
  DiscordBotMismatchError,
  NodeUnavailableError,
  ProtocolError,
  RemediaRpcError,
  SessionLostError,
  UnsupportedMediaSourceError,
  asError,
} from './errors.js';
import { parseRemoteEndpoint } from './endpoint.js';
import { RpcClient, type RpcLifecycleEvent } from './rpc.js';
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
  RemoteNodeConfig,
  ServerLifecycleEvent,
  VoiceConnectionState,
} from './types.js';
import type { GuildBackend, NodeAdapter } from './adapters.js';

const PROTOCOL_VERSION = 1;
const MAX_TERMINAL_EVENTS = 128;
const RESUME_ATTEMPTS = 4;
const HEARTBEAT_INTERVAL_MS = 20_000;
const HEARTBEAT_TIMEOUT_MS = 10_000;

interface RemoteNodeEvents {
  status: [{ snapshot: NodeSnapshot; error: Error | null }];
  lifecycle: [ServerLifecycleEvent];
  resumed: [{ snapshot: NodeSnapshot; guildIds: readonly string[] }];
  sessionLost: [{ snapshot: NodeSnapshot; guildIds: readonly string[]; reason: string }];
  guildLost: [{ guildId: string }];
}

interface NodeInfo {
  nodeId: string;
  version: string;
  protocolVersion: number;
}

interface NodeStatsResult {
  nodeId: string;
  protocolVersion: number;
  players: { active: number; maximum: number };
  connections?: { active: number; maximum: number };
  transportHealth?: string;
  processingHealth?: string;
  cpu?: Record<string, number | null>;
  memory?: Record<string, number | null>;
}

interface NodeHealthResult {
  status: string;
}

interface GuildSnapshot {
  guildSessionId: string;
  guildId: string;
  botUserId: string;
  playback: Record<string, unknown>;
  voiceState: string | null;
}

interface PlaybackWaiter {
  guildId: string;
  generation: number;
  resolve(value: PlaybackCompletion): void;
  reject(error: Error): void;
}

export interface RemoteNodeOptions {
  config: RemoteNodeConfig;
  requestTimeoutMs: number;
  maxPendingRequests: number;
}

/** Direct protocol-v1 adapter for one resumable remedia-server node. */
export class RemoteNodeAdapter extends EventEmitter<RemoteNodeEvents> implements NodeAdapter {
  readonly id: string;
  readonly type = 'remote' as const;
  readonly #endpoint: ReturnType<typeof parseRemoteEndpoint>;
  readonly #requestTimeoutMs: number;
  readonly #maxPendingRequests: number;
  readonly #guilds = new Set<string>();
  readonly #terminal = new Map<string, PlaybackCompletion>();
  readonly #waiters = new Set<PlaybackWaiter>();
  #status: NodeStatus = 'connecting';
  #serverNodeId: string | null = null;
  #version: string | null = null;
  #protocolVersion: number | null = null;
  #statistics: NodeStatistics | null = null;
  #lastError: string | null = null;
  #socket: WebSocket | undefined;
  #openingSocket: WebSocket | undefined;
  #rpc: RpcClient | undefined;
  #sessionId: string | undefined;
  #resumeCredential: string | undefined;
  #botUserId: string | undefined;
  #lastSequence = 0;
  #closed = false;
  #connecting = false;
  #connectWork: Promise<void> | undefined;
  #heartbeat: ReturnType<typeof setInterval> | undefined;
  #lastPongAt = 0;
  #reconnectTask: Promise<void> | undefined;
  #knownGuildSnapshots = new Map<string, GuildSnapshot>();
  #lostGuilds = new Set<string>();
  #pendingDestroyGuilds = new Set<string>();
  #pendingVoiceDisconnects = new Set<string>();
  readonly #backends = new Map<string, RemoteGuildBackend>();
  #capacityOverride: number | undefined;

  constructor(options: RemoteNodeOptions) {
    super();
    // Each live guild session subscribes to node lifecycle events, so listener count scales with
    // the node's configured player capacity.
    this.setMaxListeners(0);
    this.id = options.config.id;
    this.#endpoint = parseRemoteEndpoint(options.config.url, options.config.auth);
    this.#requestTimeoutMs = options.requestTimeoutMs;
    this.#maxPendingRequests = options.maxPendingRequests;
    this.#capacityOverride = options.config.capacity;
    if (
      this.#capacityOverride !== undefined &&
      (!Number.isSafeInteger(this.#capacityOverride) || this.#capacityOverride < 1)
    ) {
      throw new RangeError(`capacity for node ${this.id} must be a positive safe integer`);
    }
  }

  get snapshot(): NodeSnapshot {
    return {
      id: this.id,
      type: this.type,
      status: this.#status,
      nodeId: this.#serverNodeId,
      version: this.#version,
      protocolVersion: this.#protocolVersion,
      statistics: this.#statistics,
      capacity: this.#capacityOverride ?? this.#statistics?.playersMaximum ?? 0,
      assignedGuilds: [...this.#guilds],
      lastError: this.#lastError,
    };
  }

  get status(): NodeStatus {
    return this.#status;
  }

  get botUserId(): string | undefined {
    return this.#botUserId;
  }

  get assignedGuilds(): ReadonlySet<string> {
    return this.#guilds;
  }

  get authenticatedBotUserId(): string | undefined {
    return this.#botUserId;
  }

  receive(_packet: unknown): void {
    // The high-level client routes Gateway dispatches to the matching guild session.
  }

  createGuildSession(options: DiscordSessionOptions, onDispose: () => void): GuildBackend {
    if (this.#backends.has(options.guildId)) {
      throw new Error(`guild ${options.guildId} is already assigned to node ${this.id}`);
    }
    let backend: RemoteGuildBackend | undefined;
    backend = new RemoteGuildBackend(this, options, () => {
      if (this.#backends.get(options.guildId) === backend) this.#backends.delete(options.guildId);
      onDispose();
    });
    this.#backends.set(options.guildId, backend);
    return backend;
  }

  async releaseGuild(guildId: string): Promise<void> {
    const backend = this.#backends.get(guildId);
    if (backend) await backend.dispose();
    else await this.destroyGuild(guildId);
  }

  async start(): Promise<void> {
    if (this.#closed) return;
    try {
      await this.#runConnect();
    } catch (error) {
      if (this.#closed) return;
      this.#setStatus('disconnected', asError(error));
      if (isRetryableConnectionError(error)) this.#scheduleReconnect();
    }
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    if (this.#heartbeat) clearInterval(this.#heartbeat);
    this.#heartbeat = undefined;
    const rpc = this.#rpc;
    const socket = this.#socket ?? this.#openingSocket;
    this.#rpc = undefined;
    this.#socket = undefined;
    this.#openingSocket = undefined;
    if (rpc && socket?.readyState === WebSocket.OPEN && !this.#connecting) {
      await rpc.request('client.close', {}).catch(() => undefined);
    }
    rpc?.fail(new NodeUnavailableError(this.id, 'was closed'));
    if (socket && socket.readyState !== WebSocket.CLOSED) {
      if (socket.readyState === WebSocket.CONNECTING) socket.terminate();
      else socket.close(1000, 'client shutdown');
    }
    await this.#connectWork?.catch(() => undefined);
    for (const waiter of this.#waiters)
      waiter.reject(new NodeUnavailableError(this.id, 'was closed'));
    this.#waiters.clear();
    this.#setStatus('closed', null);
  }

  async createGuild(guildId: string): Promise<GuildSnapshot> {
    this.#assertHealthy();
    if (this.#lostGuilds.has(guildId)) {
      throw new SessionLostError(
        this.id,
        [guildId],
        'the guild assignment belonged to an expired client session',
      );
    }
    if (this.#guilds.has(guildId)) return this.getGuild(guildId);
    this.#clearTerminalForGuild(guildId);
    const result = parseGuildSnapshot(
      await this.request<unknown>('guild.create', { guildId }),
      this.id,
    );
    this.#guilds.add(guildId);
    this.#knownGuildSnapshots.set(guildId, result);
    await this.refreshStatistics();
    return result;
  }

  async getGuild(guildId: string): Promise<GuildSnapshot> {
    this.#assertHealthy();
    const result = parseGuildSnapshot(
      await this.request<unknown>('guild.get', { guildId }),
      this.id,
    );
    this.#guilds.add(guildId);
    this.#knownGuildSnapshots.set(guildId, result);
    return result;
  }

  async destroyGuild(guildId: string): Promise<void> {
    let deferred = false;
    try {
      if (this.#status === 'healthy' && this.#guilds.has(guildId)) {
        await this.request('guild.destroy', { guildId });
      } else if (
        this.#guilds.has(guildId) &&
        this.#sessionId &&
        this.#status !== 'session_lost' &&
        !this.#closed
      ) {
        this.#pendingDestroyGuilds.add(guildId);
        deferred = true;
      }
    } catch (error) {
      if (
        error instanceof NodeUnavailableError &&
        this.#guilds.has(guildId) &&
        this.#sessionId &&
        !this.#closed
      ) {
        this.#pendingDestroyGuilds.add(guildId);
        deferred = true;
      } else if (!(error instanceof RemediaRpcError && error.code === 'unknown_session')) {
        throw error;
      }
    } finally {
      this.#guilds.delete(guildId);
      this.#lostGuilds.delete(guildId);
      if (!deferred) {
        this.#pendingDestroyGuilds.delete(guildId);
        this.#pendingVoiceDisconnects.delete(guildId);
      }
      this.#knownGuildSnapshots.delete(guildId);
      this.#backends.delete(guildId);
      this.#clearTerminalForGuild(guildId);
      await this.refreshStatistics().catch(() => undefined);
    }
  }

  async connectVoice(
    guildId: string,
    channelId: string,
    credentials: { sessionId: string; token: string; endpoint: string },
  ): Promise<void> {
    this.#assertHealthy();
    await this.request('voice.connect', { guildId, channelId, ...credentials });
  }

  async disconnectVoice(guildId: string): Promise<void> {
    if (this.#status !== 'healthy') {
      if (this.#sessionId && this.#status !== 'session_lost' && !this.#closed) {
        this.#pendingVoiceDisconnects.add(guildId);
      }
      return;
    }
    try {
      await this.request('voice.disconnect', { guildId });
      this.#pendingVoiceDisconnects.delete(guildId);
    } catch (error) {
      if (error instanceof NodeUnavailableError && this.#sessionId && !this.#closed) {
        this.#pendingVoiceDisconnects.add(guildId);
        return;
      }
      if (!(error instanceof RemediaRpcError && error.code === 'unknown_session')) throw error;
    }
  }

  deferVoiceDisconnect(guildId: string): void {
    if (this.#sessionId && !this.#closed && this.#status !== 'session_lost') {
      this.#pendingVoiceDisconnects.add(guildId);
    }
  }

  async voiceState(guildId: string): Promise<{ state: string; failed: boolean }> {
    this.#assertHealthy();
    return this.request('voice.state', { guildId });
  }

  async play(
    guildId: string,
    source: { type: 'url'; url: string } | { type: 'file'; path: string },
  ): Promise<{ info: AudioStreamInfo; generation: number }> {
    this.#assertHealthy();
    const result = await this.request<Record<string, unknown>>('player.play', { guildId, source });
    if (!Number.isSafeInteger(result.generation) || (result.generation as number) < 1) {
      throw new ProtocolError(this.id, 'player.play response omitted its playback generation');
    }
    return { info: toStreamInfo(result, this.id), generation: result.generation as number };
  }

  async control(
    guildId: string,
    command: string,
    params: Record<string, unknown> = {},
  ): Promise<unknown> {
    this.#assertHealthy();
    return this.request(command, { guildId, ...params });
  }

  async playerState(guildId: string): Promise<PlaybackSnapshot> {
    this.#assertHealthy();
    const snapshot = await this.request<Record<string, unknown>>('player.state', { guildId });
    return parsePlaybackSnapshot(snapshot, this.id);
  }

  async guildSnapshot(guildId: string): Promise<GuildSnapshot> {
    return this.getGuild(guildId);
  }

  async refreshStatistics(): Promise<void> {
    if (this.#status !== 'healthy') return;
    const stats = await this.request<NodeStatsResult>('node.stats', {});
    this.#statistics = toNodeStatistics(stats, this.id);
  }

  async refreshGuilds(): Promise<void> {
    this.#assertHealthy();
    const snapshots = new Map(
      parseGuildList(await this.request<unknown>('guild.list', {}), this.id).map((guild) => [
        guild.guildId,
        guild,
      ]),
    );
    for (const guildId of [...this.#guilds]) {
      if (!snapshots.has(guildId)) {
        this.#guilds.delete(guildId);
        this.#knownGuildSnapshots.delete(guildId);
        this.#lostGuilds.add(guildId);
        this.emit('guildLost', { guildId });
      } else {
        const guild = await this.getGuild(guildId);
        const playback = parsePlaybackSnapshot(guild.playback, this.id);
        this.#observeSnapshot(guildId, playback);
      }
    }
  }

  async waitForCompletion(guildId: string, generation: number): Promise<PlaybackCompletion> {
    const key = completionKey(guildId, generation);
    const known = this.#terminal.get(key);
    if (known) return known;
    if (this.#closed || this.#status === 'session_lost') {
      return Promise.reject(
        new SessionLostError(this.id, [guildId], 'the owning client session is unavailable'),
      );
    }
    let resolveWaiter!: (completion: PlaybackCompletion) => void;
    let rejectWaiter!: (error: Error) => void;
    const wait = new Promise<PlaybackCompletion>((resolve, reject) => {
      resolveWaiter = resolve;
      rejectWaiter = reject;
    });
    const waiter: PlaybackWaiter = {
      guildId,
      generation,
      resolve: resolveWaiter,
      reject: rejectWaiter,
    };
    this.#waiters.add(waiter);
    try {
      if (this.#status === 'healthy') {
        const snapshot = await this.playerState(guildId);
        const terminal = completionFromSnapshot(snapshot, generation);
        if (terminal) this.#finishWaiter(waiter, terminal);
        else if (snapshot.generation > generation)
          this.#finishWaiter(waiter, { generation, state: 'stopped' });
      }
      return await wait;
    } catch (error) {
      if (error instanceof NodeUnavailableError && !this.#closed) return await wait;
      this.#waiters.delete(waiter);
      throw error;
    }
  }

  request<Result>(command: string, params: Record<string, unknown> = {}): Promise<Result> {
    this.#assertHealthy();
    return this.#rpc!.request<Result>(command, params);
  }

  async #connect(): Promise<void> {
    if (this.#closed || this.#connecting) return;
    this.#connecting = true;
    this.#setStatus('connecting', null);
    let socket: WebSocket | undefined;
    let didResume = false;
    try {
      socket = await openSocket(
        this.id,
        this.#endpoint.url,
        this.#endpoint.authorization,
        this.#requestTimeoutMs,
        (openingSocket) => {
          this.#openingSocket = openingSocket;
        },
      );
      this.#openingSocket = undefined;
      if (this.#closed) {
        socket.close(1000, 'client shutdown');
        return;
      }
      const rpc = new RpcClient(socket, {
        nodeId: this.id,
        timeoutMs: this.#requestTimeoutMs,
        maxPending: this.#maxPendingRequests,
        onEvent: (event) => this.#handleEvent(event),
        onMalformed: (error) => this.#setLastError(error.message),
      });
      this.#socket = socket;
      this.#rpc = rpc;
      socket.on('pong', () => {
        this.#lastPongAt = Date.now();
      });
      socket.once('close', (code, reason) => this.#socketClosed(socket!, code, reason.toString()));

      const protocol = await rpc.request<{ version: number }>('node.protocol_version', {});
      if (this.#closed) return;
      if (protocol.version !== PROTOCOL_VERSION) {
        throw new ProtocolError(this.id, `server protocol ${protocol.version} is unsupported`);
      }
      const info = await rpc.request<NodeInfo>('node.info', {});
      if (this.#closed) return;
      this.#verifyNodeInfo(info);
      this.#protocolVersion = info.protocolVersion;
      this.#serverNodeId = info.nodeId;
      this.#version = info.version;

      const health = await rpc.request<NodeHealthResult>('node.health', {});
      if (this.#closed) return;
      if (health.status !== 'ok') {
        throw new NodeUnavailableError(this.id, `reported health state ${String(health.status)}`);
      }

      if (this.#sessionId && this.#resumeCredential) {
        if (this.#expectedServerNodeId && this.#expectedServerNodeId !== info.nodeId) {
          await this.#loseSession(
            `server node identity changed from ${this.#expectedServerNodeId} to ${info.nodeId}`,
          );
          this.#expectedServerNodeId = info.nodeId;
        } else {
          didResume = await this.#resumeSession(rpc);
          if (this.#closed) return;
          if (!didResume) await this.#loseSession('session resumption expired or was rejected');
        }
      }
      this.#expectedServerNodeId ??= info.nodeId;

      if (!this.#sessionId) await this.#createSession(rpc);
      if (this.#closed) return;
      this.#status = 'healthy';
      this.#lastError = null;
      await this.#flushPendingCleanup(rpc);
      if (this.#closed) return;
      await this.refreshStatistics();
      if (this.#closed) return;
      await this.#reconcileAfterConnect(didResume);
      if (this.#closed) return;
      this.#startHeartbeat(socket);
      if (didResume) {
        this.emit('resumed', { snapshot: this.snapshot, guildIds: [...this.#guilds] });
      }
      this.emit('status', { snapshot: this.snapshot, error: null });
    } catch (error) {
      this.#rpc?.fail(asError(error));
      if (socket && socket.readyState !== WebSocket.CLOSED) socket.close(1011, 'connection failed');
      if (this.#socket === socket) this.#socket = undefined;
      if (this.#openingSocket === socket) this.#openingSocket = undefined;
      if (this.#rpc) this.#rpc = undefined;
      throw error;
    } finally {
      this.#connecting = false;
    }
  }

  #runConnect(): Promise<void> {
    if (this.#connectWork) return this.#connectWork;
    const work = this.#connect();
    this.#connectWork = work;
    void work
      .finally(() => {
        if (this.#connectWork === work) this.#connectWork = undefined;
      })
      .catch(() => undefined);
    return work;
  }

  #expectedServerNodeId: string | null = null;

  async #flushPendingCleanup(rpc: RpcClient): Promise<void> {
    for (const guildId of [...this.#pendingDestroyGuilds]) {
      this.#guilds.delete(guildId);
      this.#knownGuildSnapshots.delete(guildId);
      this.#lostGuilds.delete(guildId);
      try {
        await rpc.request('guild.destroy', { guildId });
      } catch (error) {
        if (!(error instanceof RemediaRpcError && error.code === 'unknown_session')) throw error;
      }
      this.#pendingDestroyGuilds.delete(guildId);
      this.#pendingVoiceDisconnects.delete(guildId);
    }
    for (const guildId of [...this.#pendingVoiceDisconnects]) {
      try {
        await rpc.request('voice.disconnect', { guildId });
      } catch (error) {
        if (!(error instanceof RemediaRpcError && error.code === 'unknown_session')) throw error;
      }
      this.#pendingVoiceDisconnects.delete(guildId);
    }
  }

  async #createSession(rpc: RpcClient): Promise<void> {
    const created = await rpc.request<{
      sessionId: string;
      resumeCredential: string;
      botUserId: string;
    }>('client.create', {});
    if (!created.sessionId || !created.resumeCredential || !isSnowflake(created.botUserId)) {
      throw new ProtocolError(this.id, 'client.create returned malformed session identity');
    }
    this.#sessionId = created.sessionId;
    this.#resumeCredential = created.resumeCredential;
    this.#botUserId = created.botUserId;
  }

  async #resumeSession(rpc: RpcClient): Promise<boolean> {
    for (let attempt = 0; attempt < RESUME_ATTEMPTS; attempt += 1) {
      try {
        const resumed = await rpc.request<unknown>('client.resume', {
          sessionId: this.#sessionId,
          resumeCredential: this.#resumeCredential,
        });
        if (
          !isRecord(resumed) ||
          typeof resumed.sessionId !== 'string' ||
          typeof resumed.resumeCredential !== 'string' ||
          !resumed.resumeCredential ||
          !isRecord(resumed.snapshot) ||
          !isSnowflake(resumed.snapshot.botUserId) ||
          !Array.isArray(resumed.snapshot.guilds)
        ) {
          throw new ProtocolError(this.id, 'client.resume returned an invalid session snapshot');
        }
        if (resumed.sessionId !== this.#sessionId) {
          throw new ProtocolError(this.id, 'client.resume returned a different session identity');
        }
        if (resumed.snapshot.botUserId !== this.#botUserId) return false;
        const guilds = resumed.snapshot.guilds.map((guild) => parseGuildSnapshot(guild, this.id));
        this.#resumeCredential = resumed.resumeCredential;
        const resumedGuilds = new Set(guilds.map((guild) => guild.guildId));
        for (const guildId of [...this.#guilds]) {
          if (!resumedGuilds.has(guildId)) {
            this.#lostGuilds.add(guildId);
            this.emit('guildLost', { guildId });
          }
        }
        this.#guilds.clear();
        for (const guild of guilds) {
          this.#guilds.add(guild.guildId);
          this.#knownGuildSnapshots.set(guild.guildId, guild);
        }
        return true;
      } catch (error) {
        if (
          !(error instanceof RemediaRpcError) ||
          (error.code !== 'unknown_session' && error.code !== 'invalid_state')
        )
          throw error;
        if (attempt + 1 < RESUME_ATTEMPTS) await delay(200 * (attempt + 1));
        else if (error.code === 'invalid_state') {
          throw new NodeUnavailableError(
            this.id,
            'the previous WebSocket is still attached; resumption will retry',
          );
        }
      }
    }
    return false;
  }

  async #loseSession(reason: string): Promise<void> {
    const guildIds = [...this.#guilds];
    for (const guildId of guildIds) this.#lostGuilds.add(guildId);
    this.#guilds.clear();
    this.#knownGuildSnapshots.clear();
    this.#pendingDestroyGuilds.clear();
    this.#pendingVoiceDisconnects.clear();
    this.#sessionId = undefined;
    this.#resumeCredential = undefined;
    this.#botUserId = undefined;
    this.#lastSequence = 0;
    this.#rejectWaiters(new SessionLostError(this.id, guildIds, reason));
    for (const backend of this.#backends.values()) backend.markSessionLost(reason);
    this.#status = 'session_lost';
    this.#lastError = reason;
    this.emit('sessionLost', { snapshot: this.snapshot, guildIds, reason });
  }

  async #reconcileAfterConnect(resumed: boolean): Promise<void> {
    if (!this.#sessionId) return;
    if (resumed) {
      const returned = new Map(
        parseGuildList(await this.#rpc!.request<unknown>('guild.list', {}), this.id).map((item) => [
          item.guildId,
          item,
        ]),
      );
      for (const guildId of [...this.#guilds]) {
        if (!returned.has(guildId)) {
          this.#guilds.delete(guildId);
          this.#knownGuildSnapshots.delete(guildId);
          this.#lostGuilds.add(guildId);
          this.emit('guildLost', { guildId });
          continue;
        }
        const guild = parseGuildSnapshot(
          await this.#rpc!.request<unknown>('guild.get', { guildId }),
          this.id,
        );
        const state = await this.#rpc!.request<Record<string, unknown>>('player.state', {
          guildId,
        });
        guild.playback = state;
        this.#knownGuildSnapshots.set(guildId, guild);
        this.#observeSnapshot(guildId, parsePlaybackSnapshot(state, this.id));
        this.#backends.get(guildId)?.applySnapshot(guild);
      }
    }
  }

  #handleEvent(event: RpcLifecycleEvent): void {
    if (event.sessionId !== this.#sessionId) return;
    if (this.#lastSequence && event.sequence > this.#lastSequence + 1) {
      void this.#reconcile().catch((error: unknown) => this.#setLastError(asError(error).message));
    }
    if (event.sequence <= this.#lastSequence) return;
    this.#lastSequence = event.sequence;
    const lifecycle: ServerLifecycleEvent = {
      nodeId: this.id,
      event: event.event,
      sessionId: event.sessionId,
      sequence: event.sequence,
      data: event.data,
    };
    this.emit('lifecycle', lifecycle);
    if (event.event === 'client.events_lost') {
      const count = asNonnegativeInteger(event.data.count) ?? 0;
      this.#setLastError(`${count} server lifecycle events were lost`);
      void this.#reconcile().catch((error: unknown) => this.#setLastError(asError(error).message));
      return;
    }
    const guildId = typeof event.data.guildId === 'string' ? event.data.guildId : undefined;
    const generation = asNonnegativeInteger(event.data.generation);
    if (!guildId || generation === undefined) return;
    if (
      event.event === 'player.completed' ||
      event.event === 'player.failed' ||
      event.event === 'player.stopped'
    ) {
      const state = event.event.slice('player.'.length) as 'completed' | 'failed' | 'stopped';
      this.#recordTerminal({ guildId, generation, state });
      this.#backends.get(guildId)?.applyTerminalEvent(event.event, generation, event.data);
    } else if (event.event === 'player.started') {
      for (const waiter of this.#waiters) {
        if (waiter.guildId === guildId && waiter.generation < generation) {
          this.#finishWaiter(waiter, { generation: waiter.generation, state: 'stopped' });
        }
      }
    }
  }

  async #reconcile(): Promise<void> {
    if (this.#status !== 'healthy' || !this.#sessionId) return;
    const current = new Map(
      parseGuildList(await this.#rpc!.request<unknown>('guild.list', {}), this.id).map((guild) => [
        guild.guildId,
        guild,
      ]),
    );
    for (const guildId of [...this.#guilds]) {
      if (!current.has(guildId)) {
        this.#guilds.delete(guildId);
        this.#lostGuilds.add(guildId);
        this.emit('guildLost', { guildId });
        continue;
      }
      const guild = parseGuildSnapshot(
        await this.#rpc!.request<unknown>('guild.get', { guildId }),
        this.id,
      );
      const playback = await this.#rpc!.request<Record<string, unknown>>('player.state', {
        guildId,
      });
      guild.playback = playback;
      this.#knownGuildSnapshots.set(guildId, guild);
      this.#observeSnapshot(guildId, parsePlaybackSnapshot(playback, this.id));
      this.#backends.get(guildId)?.applySnapshot(guild);
    }
  }

  #observeSnapshot(guildId: string, snapshot: PlaybackSnapshot): void {
    for (const state of ['completed', 'failed', 'stopped'] as const) {
      if (snapshot.state === state)
        this.#recordTerminal({ guildId, generation: snapshot.generation, state });
    }
    for (const waiter of this.#waiters) {
      const terminal = completionFromSnapshot(snapshot, waiter.generation);
      if (terminal) this.#finishWaiter(waiter, terminal);
      else if (snapshot.generation > waiter.generation) {
        this.#finishWaiter(waiter, { generation: waiter.generation, state: 'stopped' });
      }
    }
  }

  #recordTerminal(completion: PlaybackCompletion & { guildId: string }): void {
    const { guildId, ...terminal } = completion;
    const key = completionKey(guildId, terminal.generation);
    this.#terminal.delete(key);
    this.#terminal.set(key, terminal);
    while (this.#terminal.size > MAX_TERMINAL_EVENTS) {
      const oldest = this.#terminal.keys().next().value;
      if (oldest === undefined) break;
      this.#terminal.delete(oldest);
    }
    for (const waiter of this.#waiters) {
      if (waiter.guildId === guildId && waiter.generation === terminal.generation) {
        this.#finishWaiter(waiter, terminal);
      }
    }
  }

  #clearTerminalForGuild(guildId: string): void {
    const prefix = `${guildId}:`;
    for (const key of this.#terminal.keys()) {
      if (key.startsWith(prefix)) this.#terminal.delete(key);
    }
  }

  #finishWaiter(waiter: PlaybackWaiter, completion: PlaybackCompletion): void {
    if (!this.#waiters.delete(waiter)) return;
    waiter.resolve(completion);
  }

  #rejectWaiters(error: Error): void {
    for (const waiter of this.#waiters) waiter.reject(error);
    this.#waiters.clear();
  }

  #verifyNodeInfo(info: NodeInfo): void {
    if (
      !info ||
      typeof info.nodeId !== 'string' ||
      typeof info.version !== 'string' ||
      !Number.isSafeInteger(info.protocolVersion)
    ) {
      throw new ProtocolError(this.id, 'node.info response is malformed');
    }
    if (info.protocolVersion !== PROTOCOL_VERSION) {
      throw new ProtocolError(this.id, `server protocol ${info.protocolVersion} is unsupported`);
    }
  }

  #socketClosed(socket: WebSocket, code: number, reason: string): void {
    if (this.#socket !== socket || this.#closed) return;
    if (this.#heartbeat) clearInterval(this.#heartbeat);
    this.#heartbeat = undefined;
    this.#socket = undefined;
    this.#rpc?.fail(new NodeUnavailableError(this.id, 'connection was lost'));
    this.#rpc = undefined;
    const error =
      code === 1000 ? null : new NodeUnavailableError(this.id, reason || 'connection was lost');
    this.#setStatus('disconnected', error);
    this.#scheduleReconnect();
  }

  #scheduleReconnect(): void {
    if (this.#closed || this.#reconnectTask) return;
    this.#reconnectTask = (async () => {
      let attempt = 0;
      while (!this.#closed && this.#status !== 'healthy') {
        const backoff = Math.min(10_000, 500 * 2 ** Math.min(attempt, 5));
        await delay(backoff * (0.75 + Math.random() * 0.5));
        if (this.#closed) break;
        try {
          await this.#runConnect();
          if (this.status === 'healthy') break;
        } catch (error) {
          if (this.#closed) break;
          this.#setStatus('disconnected', asError(error));
          if (!isRetryableConnectionError(error)) break;
        }
        attempt += 1;
      }
      this.#reconnectTask = undefined;
    })();
  }

  #startHeartbeat(socket: WebSocket): void {
    if (this.#heartbeat) clearInterval(this.#heartbeat);
    this.#lastPongAt = Date.now();
    this.#heartbeat = setInterval(() => {
      if (this.#closed || socket !== this.#socket || socket.readyState !== WebSocket.OPEN) return;
      if (Date.now() - this.#lastPongAt > HEARTBEAT_INTERVAL_MS + HEARTBEAT_TIMEOUT_MS) {
        socket.terminate();
        return;
      }
      socket.ping();
    }, HEARTBEAT_INTERVAL_MS);
  }

  #setStatus(status: NodeStatus, error: Error | null): void {
    if (this.#status === status && (error?.message ?? null) === this.#lastError) return;
    this.#status = status;
    this.#lastError = error?.message ?? null;
    this.emit('status', { snapshot: this.snapshot, error });
  }

  #setLastError(message: string): void {
    this.#lastError = message;
  }

  #assertHealthy(): void {
    if (this.#status !== 'healthy' || !this.#rpc) throw new NodeUnavailableError(this.id);
  }
}

class RemoteGuildBackend implements GuildBackend {
  readonly nodeType = 'remote' as const;
  readonly #node: RemoteNodeAdapter;
  readonly #options: DiscordSessionOptions;
  readonly #onDispose: () => void;
  readonly #lifecycleListener: (event: ServerLifecycleEvent) => void;
  readonly #statusListener: (event: { snapshot: NodeSnapshot; error: Error | null }) => void;
  #snapshot = emptyPlaybackSnapshot();
  #connectionState: VoiceConnectionState = 'disconnected';
  #connectionError: string | null = null;
  #guildCreated = false;
  #disposed = false;
  #lost = false;

  constructor(node: RemoteNodeAdapter, options: DiscordSessionOptions, onDispose: () => void) {
    this.#node = node;
    this.#options = options;
    this.#onDispose = onDispose;
    this.#lifecycleListener = (event) => this.#handleEvent(event);
    this.#statusListener = ({ snapshot }) => {
      if (snapshot.status === 'disconnected' && this.#connectionState === 'connected') {
        this.#connectionState = 'reconnecting';
      }
    };
    node.on('lifecycle', this.#lifecycleListener);
    node.on('status', this.#statusListener);
  }

  get nodeId(): string {
    return this.#node.id;
  }
  get state(): PlaybackState {
    return this.#lost ? 'failed' : this.#snapshot.state;
  }
  get position(): PlaybackPosition {
    return this.#snapshot.position;
  }
  get durationMs(): number | null {
    return this.#snapshot.durationMs;
  }
  get error(): string | null {
    return this.#snapshot.lastError?.message ?? this.#connectionError;
  }
  get volume(): number {
    return this.#snapshot.volume;
  }
  get filterGraph(): string | null {
    return this.#snapshot.filterGraph;
  }
  get connectionState(): VoiceConnectionState {
    return this.#lost ? 'failed' : this.#connectionState;
  }
  get snapshot(): PlaybackSnapshot {
    return this.#snapshot;
  }

  async connect(): Promise<void> {
    throw new Error('remote Discord sessions require fresh Gateway voice credentials');
  }

  validateIdentity(userId: string): void {
    const configured = this.#node.authenticatedBotUserId;
    if (configured !== userId) {
      throw new DiscordBotMismatchError(this.#node.id, userId, configured ?? 'unknown');
    }
  }

  async connectWithCredentials(credentials: {
    sessionId: string;
    token: string;
    endpoint: string;
  }): Promise<void> {
    this.#assertOpen();
    this.validateIdentity(this.#options.userId);
    const configuredFilter = this.#snapshot.filterGraph;
    const configuredVolume = this.#snapshot.volume;
    const guild = await this.#node.createGuild(this.#options.guildId);
    this.#guildCreated = true;
    this.#snapshot = parsePlaybackSnapshot(guild.playback, this.#node.id);
    this.#snapshot = { ...this.#snapshot, filterGraph: configuredFilter, volume: configuredVolume };
    if (this.#snapshot.filterGraph !== null) {
      await this.#node.control(this.#options.guildId, 'player.filter.set', {
        graph: this.#snapshot.filterGraph,
      });
    }
    if (this.#snapshot.volume !== 1) {
      await this.#node.control(this.#options.guildId, 'player.volume.set', {
        gain: this.#snapshot.volume,
      });
    }
    this.#connectionState = 'negotiating_dave';
    try {
      await this.#node.connectVoice(this.#options.guildId, this.#options.channelId, credentials);
      this.#connectionState = 'connected';
      this.#connectionError = null;
      await this.refresh();
    } catch (error) {
      this.#connectionState = 'failed';
      this.#connectionError = asError(error).message;
      throw error;
    }
  }

  async disconnect(): Promise<void> {
    if (this.#disposed) return;
    if (!this.#lost && this.#guildCreated) {
      if (this.#node.status === 'healthy') await this.#node.disconnectVoice(this.#options.guildId);
      else this.#node.deferVoiceDisconnect(this.#options.guildId);
    }
    this.#connectionState = 'disconnected';
    if (this.#snapshot.state === 'playing' || this.#snapshot.state === 'paused') {
      this.#snapshot = { ...this.#snapshot, state: 'stopped' };
    }
  }

  async dispose(): Promise<void> {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#node.off('lifecycle', this.#lifecycleListener);
    this.#node.off('status', this.#statusListener);
    try {
      await this.#node.destroyGuild(this.#options.guildId);
    } finally {
      this.#onDispose();
    }
  }

  async play(source: MediaSource): Promise<{ info: AudioStreamInfo; generation: number }> {
    this.#assertGuildReady();
    if ('buffer' in source || 'stream' in source) {
      throw new UnsupportedMediaSourceError(
        'remote Remedia nodes accept only server-accessible { url } and { file } media sources',
      );
    }
    const remoteSource =
      'url' in source
        ? { type: 'url' as const, url: source.url }
        : { type: 'file' as const, path: source.file };
    const result = await this.#node.play(this.#options.guildId, remoteSource);
    const terminalBeforeResponse =
      this.#snapshot.generation === result.generation &&
      (this.#snapshot.state === 'completed' ||
        this.#snapshot.state === 'failed' ||
        this.#snapshot.state === 'stopped');
    this.#snapshot = {
      ...this.#snapshot,
      state: terminalBeforeResponse ? this.#snapshot.state : 'playing',
      generation: result.generation,
      source: result.info,
      durationMs: result.info.durationMs,
      lastError: terminalBeforeResponse ? this.#snapshot.lastError : null,
    };
    return result;
  }

  async pause(): Promise<void> {
    this.#assertGuildReady();
    await this.#node.control(this.#options.guildId, 'player.pause');
    this.#snapshot = { ...this.#snapshot, state: 'paused' };
  }

  async resume(): Promise<void> {
    this.#assertGuildReady();
    await this.#node.control(this.#options.guildId, 'player.resume');
    this.#snapshot = { ...this.#snapshot, state: 'playing' };
  }

  async seek(positionMs: number): Promise<void> {
    this.#assertGuildReady();
    await this.#node.control(this.#options.guildId, 'player.seek', { positionMs });
    await this.refresh();
  }

  async stop(): Promise<void> {
    this.#assertGuildReady();
    await this.#node.control(this.#options.guildId, 'player.stop');
    this.#snapshot = { ...this.#snapshot, state: 'stopped' };
  }

  async wait(): Promise<PlaybackCompletion> {
    this.#assertOpen();
    if (!this.#guildCreated) throw new Error('remote Discord session is not connected');
    return this.#node.waitForCompletion(this.#options.guildId, this.#snapshot.generation);
  }

  async setFilterGraph(graph: string | null): Promise<void> {
    this.#assertOpen();
    if (this.#guildCreated) {
      if (graph === null) await this.#node.control(this.#options.guildId, 'player.filter.clear');
      else await this.#node.control(this.#options.guildId, 'player.filter.set', { graph });
    }
    this.#snapshot = { ...this.#snapshot, filterGraph: graph };
  }

  async setVolume(gain: number): Promise<void> {
    this.#assertOpen();
    if (this.#guildCreated)
      await this.#node.control(this.#options.guildId, 'player.volume.set', { gain });
    this.#snapshot = { ...this.#snapshot, volume: gain };
  }

  async refresh(): Promise<PlaybackSnapshot> {
    this.#assertGuildReady();
    const guild = await this.#node.guildSnapshot(this.#options.guildId);
    this.#snapshot = parsePlaybackSnapshot(guild.playback, this.#node.id);
    this.#connectionState = normalizeVoiceState(guild.voiceState);
    this.#connectionError = null;
    return this.#snapshot;
  }

  /** @internal Update cached state from an authoritative reconnect snapshot. */
  applySnapshot(guild: GuildSnapshot): void {
    this.#guildCreated = true;
    this.#lost = false;
    this.#snapshot = parsePlaybackSnapshot(guild.playback, this.#node.id);
    this.#connectionState = normalizeVoiceState(guild.voiceState);
    this.#connectionError = null;
  }

  /** @internal Apply a generation-correlated terminal event, including events preceding RPC replies. */
  applyTerminalEvent(
    event: 'player.completed' | 'player.failed' | 'player.stopped',
    generation: number,
    data: Readonly<Record<string, unknown>>,
  ): void {
    if (generation < this.#snapshot.generation) return;
    const state = event.slice('player.'.length) as 'completed' | 'failed' | 'stopped';
    const code = typeof data.code === 'string' ? data.code : 'playback_failed';
    this.#snapshot = {
      ...this.#snapshot,
      generation,
      state,
      lastError:
        state === 'failed'
          ? { code, message: 'Playback failed; see server diagnostics for details.' }
          : state === 'completed'
            ? null
            : this.#snapshot.lastError,
    };
  }

  markSessionLost(reason: string): void {
    this.#lost = true;
    this.#connectionState = 'failed';
    this.#connectionError = reason;
    this.#snapshot = {
      ...this.#snapshot,
      state: 'failed',
      lastError: { code: 'unknown_session', message: 'The remote server session was lost.' },
    };
  }

  #handleEvent(event: ServerLifecycleEvent): void {
    if (event.data.guildId !== this.#options.guildId) return;
    if (event.event === 'voice.state_changed' && typeof event.data.state === 'string') {
      this.#connectionState = normalizeVoiceState(event.data.state);
    } else if (event.event === 'voice.failed') {
      this.#connectionState = 'failed';
      this.#connectionError = 'Discord voice transport failed';
    } else if (event.event === 'player.started') {
      const generation = asNonnegativeInteger(event.data.generation);
      if (generation !== undefined) {
        const source = isRecord(event.data.source) ? event.data.source : undefined;
        this.#snapshot = {
          ...this.#snapshot,
          generation,
          state: 'playing',
          source: source ? toStreamInfo(source, this.#node.id) : this.#snapshot.source,
          durationMs: source ? nullableNumber(source.durationMs) : this.#snapshot.durationMs,
          lastError: null,
        };
      }
    } else if (
      event.event === 'player.completed' ||
      event.event === 'player.failed' ||
      event.event === 'player.stopped'
    ) {
      const generation = asNonnegativeInteger(event.data.generation);
      if (generation === this.#snapshot.generation) {
        const state =
          event.event === 'player.completed'
            ? 'completed'
            : event.event === 'player.failed'
              ? 'failed'
              : 'stopped';
        this.#snapshot = { ...this.#snapshot, state };
      }
    } else if (event.event === 'player.paused') {
      this.#snapshot = { ...this.#snapshot, state: 'paused' };
    } else if (event.event === 'player.resumed') {
      this.#snapshot = { ...this.#snapshot, state: 'playing' };
    } else if (event.event === 'player.volume_changed' && typeof event.data.gain === 'number') {
      this.#snapshot = { ...this.#snapshot, volume: event.data.gain };
    }
  }

  #assertOpen(): void {
    if (this.#disposed) throw new Error('Discord session has been disposed');
    if (this.#lost)
      throw new SessionLostError(
        this.#node.id,
        [this.#options.guildId],
        'the owning client session expired',
      );
  }

  #assertGuildReady(): void {
    this.#assertOpen();
    if (!this.#guildCreated) throw new Error('remote Discord session is not connected');
    if (this.#node.status !== 'healthy') throw new NodeUnavailableError(this.#node.id);
  }
}

async function openSocket(
  nodeId: string,
  url: string,
  authorization: string,
  timeoutMs: number,
  onSocketCreated: (socket: WebSocket) => void,
): Promise<WebSocket> {
  const socket = new WebSocket(url, { headers: { authorization } });
  onSocketCreated(socket);
  return new Promise((resolve, reject) => {
    let settled = false;
    const timer = setTimeout(
      () => finish(new NodeUnavailableError(nodeId, 'connection timed out')),
      timeoutMs,
    );
    const finish = (error?: Error): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.off('open', onOpen);
      socket.off('error', onError);
      socket.off('close', onClose);
      socket.off('unexpected-response', onUnexpectedResponse);
      if (error) {
        // ws may emit an error after aborting a handshake; retain a listener on this failed socket.
        socket.on('error', () => undefined);
        socket.terminate();
        reject(error);
      } else {
        resolve(socket);
      }
    };
    const onOpen = (): void => finish();
    const onError = (_error: Error): void =>
      finish(new NodeUnavailableError(nodeId, 'could not establish a WebSocket connection'));
    const onClose = (): void =>
      finish(new NodeUnavailableError(nodeId, 'connection closed during handshake'));
    const onUnexpectedResponse = (
      _request: import('node:http').ClientRequest,
      response: import('node:http').IncomingMessage,
    ): void => {
      response.resume();
      if (response.statusCode === 401) {
        finish(
          new RemediaRpcError(nodeId, 'authentication_failed', 'WebSocket authentication failed'),
        );
      } else {
        finish(
          new NodeUnavailableError(
            nodeId,
            `WebSocket handshake was rejected with HTTP ${response.statusCode ?? 'error'}`,
          ),
        );
      }
    };
    socket.once('open', onOpen);
    socket.once('error', onError);
    socket.once('close', onClose);
    socket.once('unexpected-response', onUnexpectedResponse);
  });
}

function toNodeStatistics(stats: NodeStatsResult, nodeId: string): NodeStatistics {
  if (
    !isRecord(stats) ||
    !isRecord(stats.players) ||
    !isRecord(stats.connections) ||
    !isCount(stats.players.active) ||
    !isCount(stats.players.maximum) ||
    stats.players.maximum < 1 ||
    !isCount(stats.connections.active) ||
    !isCount(stats.connections.maximum) ||
    stats.connections.maximum < 1 ||
    typeof stats.transportHealth !== 'string' ||
    typeof stats.processingHealth !== 'string' ||
    !isRecord(stats.cpu) ||
    !isRecord(stats.memory)
  ) {
    throw new ProtocolError(nodeId, 'node.stats response is malformed');
  }
  return {
    playersActive: stats.players.active,
    playersMaximum: stats.players.maximum,
    connectionsActive: stats.connections?.active ?? 0,
    connectionsMaximum: stats.connections?.maximum ?? 0,
    transportHealth: stats.transportHealth ?? 'unknown',
    processingHealth: stats.processingHealth ?? 'unknown',
    cpu: numberRecord(stats.cpu),
    memory: numberRecord(stats.memory),
  };
}

function numberRecord(value: Record<string, unknown>): Record<string, number | null> {
  return Object.fromEntries(
    Object.entries(value).map(([key, item]) => [
      key,
      typeof item === 'number' && Number.isFinite(item) ? item : null,
    ]),
  );
}

function isCount(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 0;
}

function toStreamInfo(result: Record<string, unknown>, nodeId: string): AudioStreamInfo {
  if (typeof result.codec !== 'string' || !Number.isSafeInteger(result.streamIndex)) {
    throw new ProtocolError(nodeId, 'player.play response omitted stream metadata');
  }
  return {
    streamIndex: result.streamIndex as number,
    codec: result.codec,
    sampleRate: nullableNumber(result.sampleRate),
    channels: nullableNumber(result.channels),
    channelLayout: nullableString(result.channelLayout),
    durationMs: nullableNumber(result.durationMs),
    bitRate: nullableNumber(result.bitRate),
    metadata: isRecord(result.metadata) ? stringifyRecord(result.metadata) : {},
  };
}

function parsePlaybackSnapshot(value: Record<string, unknown>, nodeId: string): PlaybackSnapshot {
  if (
    !isPlaybackState(value.state) ||
    !Number.isSafeInteger(value.generation) ||
    !isRecord(value.position)
  ) {
    throw new ProtocolError(nodeId, 'player.state snapshot is malformed');
  }
  const position = value.position;
  let mediaMs: number | null = null;
  let mediaPositionAccuracy: PlaybackSnapshot['position']['mediaPositionAccuracy'] = null;
  if (isRecord(position.media)) {
    mediaMs = nullableNumber(position.media.positionMs);
    mediaPositionAccuracy =
      position.media.accuracy === 'timestamp' || position.media.accuracy === 'estimated'
        ? position.media.accuracy
        : null;
  }
  const mode =
    value.processingMode === 'passthrough' || value.processingMode === 'transcoding'
      ? value.processingMode
      : null;
  return {
    state: value.state,
    generation: value.generation as number,
    processingMode: mode,
    position: {
      mediaMs,
      mediaPositionAccuracy,
      submittedMs: nullableNumber(position.submittedMs) ?? 0,
      playedMs: nullableNumber(position.playedMs),
    },
    durationMs: nullableNumber(value.durationMs),
    source: isRecord(value.source) ? toStreamInfo(value.source, nodeId) : null,
    filterGraph: nullableString(value.filterGraph),
    volume: typeof value.volume === 'number' ? value.volume : 1,
    lastError:
      isRecord(value.lastError) &&
      typeof value.lastError.code === 'string' &&
      typeof value.lastError.message === 'string'
        ? { code: value.lastError.code, message: value.lastError.message }
        : null,
  };
}

function completionFromSnapshot(
  snapshot: PlaybackSnapshot,
  generation: number,
): PlaybackCompletion | undefined {
  if (snapshot.generation !== generation) return undefined;
  if (
    snapshot.state === 'completed' ||
    snapshot.state === 'failed' ||
    snapshot.state === 'stopped'
  ) {
    return { generation, state: snapshot.state };
  }
  return undefined;
}

function isRetryableConnectionError(error: unknown): boolean {
  if (error instanceof ProtocolError) return false;
  if (error instanceof RemediaRpcError && error.code === 'authentication_failed') return false;
  return true;
}

function completionKey(guildId: string, generation: number): string {
  return `${guildId}:${generation}`;
}

function emptyPlaybackSnapshot(): PlaybackSnapshot {
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

function normalizeVoiceState(value: string | null): VoiceConnectionState {
  if (
    value === 'negotiating_dave' ||
    value === 'connected' ||
    value === 'reconnecting' ||
    value === 'disconnecting' ||
    value === 'failed'
  )
    return value;
  return 'disconnected';
}

function isPlaybackState(value: unknown): value is PlaybackSnapshot['state'] {
  return (
    value === 'idle' ||
    value === 'starting' ||
    value === 'playing' ||
    value === 'pausing' ||
    value === 'resuming' ||
    value === 'paused' ||
    value === 'stopping' ||
    value === 'stopped' ||
    value === 'completed' ||
    value === 'failed'
  );
}

function parseGuildList(value: unknown, nodeId: string): GuildSnapshot[] {
  if (!isRecord(value) || !Array.isArray(value.guilds)) {
    throw new ProtocolError(nodeId, 'guild.list response is malformed');
  }
  return value.guilds.map((guild) => parseGuildSnapshot(guild, nodeId));
}

function parseGuildSnapshot(value: unknown, nodeId: string): GuildSnapshot {
  if (
    !isRecord(value) ||
    typeof value.guildSessionId !== 'string' ||
    !isSnowflake(value.guildId) ||
    !isSnowflake(value.botUserId) ||
    !isRecord(value.playback) ||
    (value.voiceState !== null && typeof value.voiceState !== 'string')
  ) {
    throw new ProtocolError(nodeId, 'guild snapshot is malformed');
  }
  parsePlaybackSnapshot(value.playback, nodeId);
  return {
    guildSessionId: value.guildSessionId,
    guildId: value.guildId,
    botUserId: value.botUserId,
    playback: value.playback,
    voiceState: value.voiceState,
  };
}

function isSnowflake(value: unknown): value is string {
  if (typeof value !== 'string' || !/^\d{1,20}$/.test(value)) return false;
  const parsed = BigInt(value);
  return parsed > 0n && parsed <= 18_446_744_073_709_551_615n;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringifyRecord(value: Record<string, unknown>): Record<string, string> {
  const entries = Object.entries(value).filter(
    (entry): entry is [string, string] => typeof entry[1] === 'string',
  );
  return Object.fromEntries(entries);
}

function nullableNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function nullableString(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function asNonnegativeInteger(value: unknown): number | undefined {
  return Number.isSafeInteger(value) && (value as number) >= 0 ? (value as number) : undefined;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
