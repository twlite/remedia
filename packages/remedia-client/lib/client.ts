import { EventEmitter } from 'node:events';
import { EmbeddedNodeAdapter } from './embedded-node.js';
import { DiscordBotMismatchError, NodeSelectionError, asError } from './errors.js';
import { RemoteNodeAdapter, RemoteNodeEvent } from './remote-node.js';
import { DiscordSession, parseGatewayDispatch, type DiscordSessionRuntime } from './discord.js';
import { RemediaEventEmitter } from './events.js';
import {
  PlaybackSessionEvents as PlaybackEvent,
  RemediaClientEventNames as ClientEvent,
  VoiceSessionEvents as VoiceEvent,
} from './types.js';
import type { NodeAdapter } from './adapters.js';
import type {
  DiscordGatewayPacket,
  DiscordGatewaySend,
  DiscordSessionOptions,
  GuildAssignment,
  MediaSource,
  NodeSelection,
  NodeSnapshot,
  RemediaClientEvents,
  RemediaClientOptions,
  RemediaNodeConfig,
} from './types.js';

interface ManagedDiscordSession {
  readonly guildId: string;
  dispose(): Promise<void>;
  receiveGatewayDispatch(
    event: string,
    data: Record<string, unknown>,
    sequence?: number,
    epoch?: number,
  ): void;
  markSessionLost(reason: string): void;
}

/** High-level orchestration across embedded N-API and remote WebSocket Remedia nodes. */
export class RemediaClient
  extends RemediaEventEmitter<RemediaClientEvents>
  implements DiscordSessionRuntime
{
  readonly #send: DiscordGatewaySend;
  readonly #botUserId: string | undefined;
  readonly #nodes = new Map<string, NodeAdapter>();
  readonly #sessions = new Map<string, ManagedDiscordSession>();
  readonly #assignments = new Map<string, GuildAssignment>();
  readonly #selectNode: NodeSelection | undefined;
  #assignmentWork: Promise<void> = Promise.resolve();
  #gatewaySequence: number | undefined;
  #gatewayEpoch = 0;
  #closed = false;
  #closing = false;
  #closePromise: Promise<void> | undefined;

  private constructor(options: RemediaClientOptions) {
    super();
    this.#send = options.send;
    this.#botUserId = options.botUserId;
    this.#selectNode = options.selectNode;

    const ids = new Set<string>();
    for (const config of options.nodes) {
      validateNodeConfig(config);
      if (ids.has(config.id))
        throw new TypeError(`node id ${config.id} is configured more than once`);
      ids.add(config.id);
      const adapter =
        config.type === 'embedded'
          ? new EmbeddedNodeAdapter(config, options.send, (event) =>
              this.#handleNodeStatus(event.snapshot, event.error),
            )
          : new RemoteNodeAdapter({
              config,
              botUserId: options.botUserId ?? '',
              requestTimeoutMs: options.requestTimeoutMs ?? 15_000,
              maxPendingRequests: options.maxPendingRequests ?? 128,
            });
      this.#nodes.set(config.id, adapter);
      if (adapter instanceof RemoteNodeAdapter) this.#wireRemote(adapter);
    }
    if (this.#nodes.size === 0) throw new TypeError('at least one Remedia node must be configured');
  }

  /**
   * Create an independent engine or client and initialize its configured resources.
   * @param options - Node pool, bot Gateway callback, bot identity, and bounded remote request settings.
   * @returns The initialized instance; inspect node health before using a remote assignment.
   */
  static async create(options: RemediaClientOptions): Promise<RemediaClient> {
    validateOptions(options);
    const client = new RemediaClient(options);
    await Promise.all(
      [...client.#nodes.values()].map(async (node) => {
        try {
          await node.start();
        } catch (error) {
          client.#handleNodeStatus(node.snapshot, asError(error));
        }
      }),
    );
    return client;
  }

  /**
   * Inspect disposable media independently of guild routing. Multiple healthy nodes require an explicit nodeId.
   * @param source - Encoded media; remote nodes support only server-accessible files and HTTP(S) URLs.
   * @param options - Node selection, cancellation, and total deadline, defaulting to 15000 ms.
   * @returns Available AudioStreamInfo; rejects unavailable or ambiguous nodes, unsupported inputs, timeout, or cancellation.
   */
  async probe(
    source: MediaSource,
    options: import('./types.js').ClientProbeOptions = {},
  ): Promise<import('./types.js').AudioStreamInfo> {
    this.#assertOpen();
    const eligible = [...this.#nodes.values()].filter((node) => node.status === 'healthy');
    const node = options.nodeId
      ? this.#nodes.get(options.nodeId)
      : eligible.length === 1
        ? eligible[0]
        : undefined;
    if (!node || node.status !== 'healthy')
      throw new NodeSelectionError('probe requires one healthy node or an explicit healthy nodeId');
    return node.probe(source, options);
  }

  /** Configured embedded and remote nodes; at least one node and unique configured IDs are required. */
  get nodes(): readonly NodeSnapshot[] {
    return [...this.#nodes.values()].map((node) => node.snapshot);
  }

  /** Read-only sticky guild assignments, including assignments marked lost after expiry. */
  get assignments(): readonly GuildAssignment[] {
    return [...this.#assignments.values()].map((assignment) => ({ ...assignment }));
  }

  /**
   * Look up the sticky backend assignment for a guild.
   * @param guildId - Discord guild snowflake used to route the operation.
   * @returns A copy of the assignment, or undefined when the guild is unassigned.
   */
  getAssignment(guildId: string): GuildAssignment | undefined {
    const assignment = this.#assignments.get(guildId);
    return assignment ? { ...assignment } : undefined;
  }

  /**
   * Register one guild session with initial playback and voice configuration. Duplicate guild registration throws.
   * @param options - Guild and bot identity, requested channel, initial processing settings, and voice flags.
   * @returns The unconnected session; call connect() to request a Gateway voice join.
   * @typeParam Metadata - Application-owned metadata attached to playback generations.
   */
  createDiscordSession<Metadata = unknown>(
    options: DiscordSessionOptions,
  ): DiscordSession<Metadata> {
    this.#assertOpen();
    if (this.#botUserId && options.userId !== this.#botUserId) {
      throw new DiscordBotMismatchError('RemediaClient', this.#botUserId, options.userId);
    }
    if (this.#sessions.has(options.guildId))
      throw new Error(`a Discord session already exists for guild ${options.guildId}`);
    const session = new DiscordSession<Metadata>(this, options);
    this.#sessions.set(options.guildId, session);
    forwardSessionEvents(session, this);
    return session;
  }

  /**
   * Forward a raw bot Gateway packet to the matching voice session; unrelated packets are ignored.
   * @param packet - Raw packet from this bot's Gateway dispatch stream.
   */
  receive(packet: unknown): void {
    if (this.#closed || !isRecord(packet)) return;
    const sequence =
      typeof packet.s === 'number' && Number.isSafeInteger(packet.s) && packet.s >= 0
        ? packet.s
        : undefined;
    if (packet.t === 'READY') {
      this.#gatewayEpoch += 1;
      this.#gatewaySequence = sequence;
    } else if (
      this.#gatewaySequence === undefined ||
      (sequence !== undefined && sequence > this.#gatewaySequence)
    ) {
      if (sequence !== undefined) this.#gatewaySequence = sequence;
    }
    for (const node of this.#nodes.values()) if (node.type === 'embedded') node.receive(packet);

    const dispatch = parseGatewayDispatch(packet as DiscordGatewayPacket);
    if (!dispatch) return;
    this.#sessions
      .get(dispatch.data.guild_id as string)
      ?.receiveGatewayDispatch(
        dispatch.event,
        dispatch.data,
        dispatch.sequence,
        this.#gatewayEpoch,
      );
  }

  /**
   * Dispose owned sessions, close node resources, and release the client or engine.
   * @returns Resolves after cleanup; rejects when a resource fails to close.
   */
  close(): Promise<void> {
    if (this.#closePromise) return this.#closePromise;
    if (this.#closed) return Promise.resolve();
    this.#closing = true;
    this.#closePromise = (async () => {
      const sessions = [...this.#sessions.values()];
      const sessionResults = await Promise.allSettled(sessions.map((session) => session.dispose()));
      this.#closed = true;
      const nodeResults = await Promise.allSettled(
        [...this.#nodes.values()].map((node) => node.close()),
      );
      this.#sessions.clear();
      this.removeAllListeners();
      const failures = [...sessionResults, ...nodeResults].flatMap((result) =>
        result.status === 'rejected' ? [result.reason] : [],
      );
      if (failures.length > 0)
        throw new AggregateError(
          failures.map(asError),
          'one or more Remedia client resources failed to close',
        );
    })();
    return this.#closePromise;
  }

  /**
   * Send a voice state request through the caller-owned Discord Gateway connection.
   *
   * @internal
   * @param guildId - Discord guild snowflake used to route the operation.
   * @param channelId - Destination Discord voice channel snowflake; null requests a leave where supported.
   * @param selfDeaf - Discord self-deaf flag; defaults to true.
   * @param selfMute - Discord self-mute flag; defaults to false.
   */
  sendVoicePayload(
    guildId: string,
    channelId: string | null,
    selfDeaf = true,
    selfMute = false,
  ): void | Promise<void> {
    if (this.#closed) throw new Error('RemediaClient has been closed');
    return this.#send(guildId, {
      op: 4,
      d: { guild_id: guildId, channel_id: channelId, self_mute: selfMute, self_deaf: selfDeaf },
    });
  }

  /**
   * Read the last accepted Gateway dispatch sequence.
   *
   * @internal
   * @returns The dispatch sequence, or undefined before one is observed.
   */
  gatewaySequence(): number | undefined {
    return this.#gatewaySequence;
  }

  /**
   * Allocate or reuse a sticky guild backend under the client assignment lock.
   *
   * @internal
   * @param options - Guild identity, node preference, and retained session configuration.
   * @param onDispose - Callback invoked when this wrapper releases its owning registration.
   * @returns The assigned backend; rejects when the node is unavailable or at capacity.
   */
  async resolveBackend(options: DiscordSessionOptions, onDispose: () => void) {
    this.#assertOpen();
    return this.#withAssignmentLock(async () => {
      const assigned = this.#assignments.get(options.guildId);
      if (assigned) {
        if (assigned.status === 'lost')
          throw new NodeSelectionError(
            `guild ${options.guildId} remains assigned to lost node ${assigned.nodeId}; dispose it before creating a new session`,
          );
        const sticky = this.#nodes.get(assigned.nodeId);
        if (!sticky || sticky.status === 'closed' || sticky.status === 'session_lost') {
          throw new NodeSelectionError(
            `guild ${options.guildId} is assigned to unavailable node ${assigned.nodeId}`,
          );
        }
        return sticky.createGuildSession(options, onDispose);
      }

      const node = options.nodeId
        ? this.#getPinnedNode(options.nodeId, options.guildId)
        : await this.#chooseNode(options.guildId);
      const assignment: GuildAssignment = {
        guildId: options.guildId,
        nodeId: node.id,
        nodeType: node.type,
        status: 'assigned',
      };
      this.#assignments.set(options.guildId, assignment);
      this.emit(ClientEvent.guildAssigned, { ...assignment });
      try {
        return node.createGuildSession(options, onDispose);
      } catch (error) {
        this.#assignments.delete(options.guildId);
        throw error;
      }
    });
  }

  /**
   * Unregister the guild session and release its sticky node assignment.
   *
   * @internal
   * @param session - Guild session whose registration and assignment should be released.
   */
  releaseSession(session: {
    /** Discord guild snowflake as a decimal string; unique within the owning client. */
    readonly guildId: string;
  }): void {
    this.#sessions.delete(session.guildId);
    const assignment = this.#assignments.get(session.guildId);
    if (assignment) {
      this.#assignments.delete(session.guildId);
      this.emit(ClientEvent.guildReleased, { ...assignment });
    }
  }

  async #chooseNode(guildId: string, source?: MediaSource): Promise<NodeAdapter> {
    const healthy = [...this.#nodes.values()].filter((node) => {
      if (node.status !== 'healthy') return false;
      const snapshot = node.snapshot;
      const capacity = snapshot.capacity;
      const stats = snapshot.statistics;
      const reserved = [...this.#assignments.values()].filter(
        (assignment) => assignment.nodeId === node.id,
      ).length;
      const active = Math.max(stats?.playersActive ?? 0, snapshot.assignedGuilds.length, reserved);
      return (
        capacity > 0 &&
        active < capacity &&
        stats?.transportHealth !== 'degraded' &&
        stats?.processingHealth !== 'degraded'
      );
    });
    if (healthy.length === 0) {
      throw new NodeSelectionError('no healthy Remedia node has available player capacity');
    }

    let selected: string;
    if (this.#selectNode) {
      const context = source === undefined ? { guildId } : { guildId, source };
      selected = await this.#selectNode(
        healthy.map((node) => node.snapshot),
        context,
      );
    } else {
      healthy.sort((left, right) => {
        const leftStats = left.snapshot.statistics;
        const rightStats = right.snapshot.statistics;
        const leftReserved = [...this.#assignments.values()].filter(
          (assignment) => assignment.nodeId === left.id,
        ).length;
        const rightReserved = [...this.#assignments.values()].filter(
          (assignment) => assignment.nodeId === right.id,
        ).length;
        const leftUsage =
          Math.max(
            leftStats?.playersActive ?? 0,
            left.snapshot.assignedGuilds.length,
            leftReserved,
          ) / left.snapshot.capacity;
        const rightUsage =
          Math.max(
            rightStats?.playersActive ?? 0,
            right.snapshot.assignedGuilds.length,
            rightReserved,
          ) / right.snapshot.capacity;
        return leftUsage - rightUsage || left.id.localeCompare(right.id);
      });
      selected = healthy[0]!.id;
    }
    const node = this.#nodes.get(selected);
    if (!node || !healthy.includes(node))
      throw new NodeSelectionError(`node selector chose unavailable or full node ${selected}`);
    if (source && node.type === 'remote' && ('buffer' in source || 'stream' in source)) {
      throw new NodeSelectionError('Buffer and Readable media sources require an embedded node');
    }
    return node;
  }

  #getPinnedNode(nodeId: string, guildId: string): NodeAdapter {
    const node = this.#nodes.get(nodeId);
    if (!node) throw new NodeSelectionError(`unknown Remedia node ${nodeId}`);
    const snapshot = node.snapshot;
    const active = Math.max(
      snapshot.statistics?.playersActive ?? 0,
      snapshot.assignedGuilds.length,
      [...this.#assignments.values()].filter((assignment) => assignment.nodeId === nodeId).length,
    );
    if (
      node.status !== 'healthy' ||
      snapshot.capacity <= active ||
      snapshot.statistics?.transportHealth === 'degraded' ||
      snapshot.statistics?.processingHealth === 'degraded'
    ) {
      throw new NodeSelectionError(`Remedia node ${nodeId} is unhealthy or at capacity`);
    }
    if (node.type === 'remote' && !isSnowflake(guildId))
      throw new TypeError('remote guild ids must be Discord snowflake strings');
    return node;
  }

  #wireRemote(node: RemoteNodeAdapter): void {
    node.on(RemoteNodeEvent.status, ({ snapshot, error }) =>
      this.#handleNodeStatus(snapshot, error),
    );
    node.on(RemoteNodeEvent.lifecycle, (event) => {
      this.emit(ClientEvent.nodeEvent, event);
      if (event.event === 'client.events_lost') {
        const count = typeof event.data.count === 'number' ? event.data.count : 0;
        this.emit(ClientEvent.eventsLost, { nodeId: node.id, count });
      }
    });
    node.on(RemoteNodeEvent.resumed, ({ snapshot, guildIds }) =>
      this.emit(ClientEvent.sessionResumed, { node: snapshot, guildIds }),
    );
    node.on(RemoteNodeEvent.sessionLost, ({ snapshot, guildIds, reason }) => {
      for (const guildId of guildIds) {
        const assignment = this.#assignments.get(guildId);
        if (assignment?.nodeId === node.id)
          this.#assignments.set(guildId, { ...assignment, status: 'lost' });
        this.#sessions.get(guildId)?.markSessionLost(reason);
      }
      this.emit(ClientEvent.sessionLost, { node: snapshot, guildIds, reason });
    });
    node.on(RemoteNodeEvent.guildLost, ({ guildId }) => {
      const assignment = this.#assignments.get(guildId);
      if (assignment?.nodeId === node.id)
        this.#assignments.set(guildId, { ...assignment, status: 'lost' });
      this.#sessions
        .get(guildId)
        ?.markSessionLost('guild session is missing from the resumed server snapshot');
    });
  }

  #handleNodeStatus(snapshot: NodeSnapshot, error: Error | null): void {
    if (snapshot.status === 'healthy') this.emit(ClientEvent.nodeConnected, snapshot);
    else if (snapshot.status === 'disconnected' || snapshot.status === 'session_lost') {
      this.emit(ClientEvent.nodeDisconnected, { node: snapshot, error });
    }
  }

  #assertOpen(): void {
    if (this.#closed || this.#closing)
      throw new Error('RemediaClient is closing or has been closed');
  }

  async #withAssignmentLock<Result>(operation: () => Promise<Result>): Promise<Result> {
    const previous = this.#assignmentWork;
    let release!: () => void;
    this.#assignmentWork = new Promise<void>((resolve) => {
      release = resolve;
    });
    await previous;
    try {
      return await operation();
    } finally {
      release();
    }
  }
}

function forwardSessionEvents<Metadata>(
  session: DiscordSession<Metadata>,
  target: RemediaClient,
): void {
  const names = [
    PlaybackEvent.playbackStarted,
    PlaybackEvent.playbackCompleted,
    PlaybackEvent.playbackStopped,
    PlaybackEvent.playbackFailed,
    PlaybackEvent.playbackPaused,
    PlaybackEvent.playbackResumed,
    PlaybackEvent.playbackSeeked,
    PlaybackEvent.filterChanged,
    PlaybackEvent.volumeChanged,
    PlaybackEvent.sessionDisposed,
    VoiceEvent.voiceStateChanged,
    VoiceEvent.voiceFailed,
  ] as const;
  const source = session as unknown as EventEmitter;
  const destination = target as unknown as EventEmitter;
  const unsubs: Array<() => void> = [];
  for (const name of names) {
    const listener = (payload: unknown): void => {
      destination.emit(name, payload);
    };
    source.on(name, listener);
    unsubs.push(() => source.off(name, listener));
  }
  const detach = (): void => {
    source.off(PlaybackEvent.sessionDisposed, detach);
    for (const unsubscribe of unsubs.splice(0)) unsubscribe();
  };
  source.once(PlaybackEvent.sessionDisposed, detach);
}

function validateOptions(options: RemediaClientOptions): void {
  if (!options || !Array.isArray(options.nodes) || options.nodes.length === 0)
    throw new TypeError('nodes must contain at least one node');
  if (typeof options.send !== 'function')
    throw new TypeError('send must be a Discord Gateway callback');
  const hasRemoteNode = options.nodes.some((node) => node?.type === 'remote');
  if (hasRemoteNode && !isSnowflake(options.botUserId ?? '')) {
    throw new TypeError(
      'botUserId must be a positive decimal Discord snowflake when using remote nodes',
    );
  }
  if (options.botUserId !== undefined && !isSnowflake(options.botUserId)) {
    throw new TypeError('botUserId must be a positive decimal Discord snowflake');
  }
  if (
    options.requestTimeoutMs !== undefined &&
    (!Number.isSafeInteger(options.requestTimeoutMs) || options.requestTimeoutMs < 1)
  ) {
    throw new RangeError('requestTimeoutMs must be a positive safe integer');
  }
  if (
    options.maxPendingRequests !== undefined &&
    (!Number.isSafeInteger(options.maxPendingRequests) || options.maxPendingRequests < 1)
  ) {
    throw new RangeError('maxPendingRequests must be a positive safe integer');
  }
}

function validateNodeConfig(config: RemediaNodeConfig): void {
  if (!config || typeof config.id !== 'string' || !config.id.trim())
    throw new TypeError('each node requires a non-empty id');
  if (config.type === 'remote' && (!config.url || typeof config.url !== 'string'))
    throw new TypeError(`remote node ${config.id} requires a URL`);
  if (config.type !== 'remote' && config.type !== 'embedded') {
    const unknown = config as { id: string; type: string };
    throw new TypeError(`node ${unknown.id} has unsupported type ${unknown.type}`);
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isSnowflake(value: string): boolean {
  if (typeof value !== 'string' || !/^\d{1,20}$/.test(value)) return false;
  try {
    return BigInt(value) > 0n && BigInt(value) <= 18_446_744_073_709_551_615n;
  } catch {
    return false;
  }
}
