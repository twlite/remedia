import { EventEmitter } from 'node:events';
import { EmbeddedNodeAdapter } from './embedded-node.js';
import { NodeSelectionError, asError } from './errors.js';
import { RemoteNodeAdapter } from './remote-node.js';
import { DiscordSession, parseGatewayDispatch, type DiscordSessionRuntime } from './discord.js';
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

/** High-level orchestration across embedded N-API and remote WebSocket Remedia nodes. */
export class RemediaClient
  extends EventEmitter<RemediaClientEvents>
  implements DiscordSessionRuntime
{
  readonly #send: DiscordGatewaySend;
  readonly #nodes = new Map<string, NodeAdapter>();
  readonly #sessions = new Map<string, DiscordSession>();
  readonly #assignments = new Map<string, GuildAssignment>();
  readonly #selectNode: NodeSelection | undefined;
  #assignmentWork: Promise<void> = Promise.resolve();
  #gatewaySequence: number | undefined;
  #closed = false;
  #closing = false;
  #closePromise: Promise<void> | undefined;

  private constructor(options: RemediaClientOptions) {
    super();
    this.#send = options.send;
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
              requestTimeoutMs: options.requestTimeoutMs ?? 15_000,
              maxPendingRequests: options.maxPendingRequests ?? 128,
            });
      this.#nodes.set(config.id, adapter);
      if (adapter instanceof RemoteNodeAdapter) this.#wireRemote(adapter);
    }
    if (this.#nodes.size === 0) throw new TypeError('at least one Remedia node must be configured');
  }

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

  get nodes(): readonly NodeSnapshot[] {
    return [...this.#nodes.values()].map((node) => node.snapshot);
  }

  /** Read-only sticky guild assignments, including assignments marked lost after expiry. */
  get assignments(): readonly GuildAssignment[] {
    return [...this.#assignments.values()].map((assignment) => ({ ...assignment }));
  }

  getAssignment(guildId: string): GuildAssignment | undefined {
    const assignment = this.#assignments.get(guildId);
    return assignment ? { ...assignment } : undefined;
  }

  createDiscordSession(options: DiscordSessionOptions): DiscordSession {
    this.#assertOpen();
    if (this.#sessions.has(options.guildId))
      throw new Error(`a Discord session already exists for guild ${options.guildId}`);
    const session = new DiscordSession(this, options);
    this.#sessions.set(options.guildId, session);
    return session;
  }

  receive(packet: unknown): void {
    if (this.#closed || !isRecord(packet)) return;
    const sequence =
      typeof packet.s === 'number' && Number.isSafeInteger(packet.s) && packet.s >= 0
        ? packet.s
        : undefined;
    if (
      typeof packet.t === 'string' &&
      (packet.t === 'READY' ||
        this.#gatewaySequence === undefined ||
        (sequence !== undefined && sequence > this.#gatewaySequence))
    ) {
      if (sequence !== undefined) this.#gatewaySequence = sequence;
    }
    for (const node of this.#nodes.values()) if (node.type === 'embedded') node.receive(packet);

    const dispatch = parseGatewayDispatch(packet as DiscordGatewayPacket);
    if (!dispatch) return;
    this.#sessions
      .get(dispatch.data.guild_id as string)
      ?.receiveGatewayDispatch(dispatch.event, dispatch.data, dispatch.sequence);
  }

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

  /** @internal */
  sendVoicePayload(guildId: string, channelId: string | null): void | Promise<void> {
    if (this.#closed) throw new Error('RemediaClient has been closed');
    return this.#send(guildId, {
      op: 4,
      d: { guild_id: guildId, channel_id: channelId, self_mute: false, self_deaf: false },
    });
  }

  /** @internal */
  gatewaySequence(): number | undefined {
    return this.#gatewaySequence;
  }

  /** @internal */
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
      this.emit('guildAssigned', { ...assignment });
      try {
        return node.createGuildSession(options, onDispose);
      } catch (error) {
        this.#assignments.delete(options.guildId);
        throw error;
      }
    });
  }

  /** @internal */
  releaseSession(session: DiscordSession): void {
    this.#sessions.delete(session.guildId);
    const assignment = this.#assignments.get(session.guildId);
    if (assignment) {
      this.#assignments.delete(session.guildId);
      this.emit('guildReleased', { ...assignment });
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
    node.on('status', ({ snapshot, error }) => this.#handleNodeStatus(snapshot, error));
    node.on('lifecycle', (event) => {
      this.emit('nodeEvent', event);
      if (event.event === 'client.events_lost') {
        const count = typeof event.data.count === 'number' ? event.data.count : 0;
        this.emit('eventsLost', { nodeId: node.id, count });
      }
    });
    node.on('resumed', ({ snapshot, guildIds }) =>
      this.emit('sessionResumed', { node: snapshot, guildIds }),
    );
    node.on('sessionLost', ({ snapshot, guildIds, reason }) => {
      for (const guildId of guildIds) {
        const assignment = this.#assignments.get(guildId);
        if (assignment?.nodeId === node.id)
          this.#assignments.set(guildId, { ...assignment, status: 'lost' });
        this.#sessions.get(guildId)?.markSessionLost(reason);
      }
      this.emit('sessionLost', { node: snapshot, guildIds, reason });
    });
    node.on('guildLost', ({ guildId }) => {
      const assignment = this.#assignments.get(guildId);
      if (assignment?.nodeId === node.id)
        this.#assignments.set(guildId, { ...assignment, status: 'lost' });
      this.#sessions
        .get(guildId)
        ?.markSessionLost('guild session is missing from the resumed server snapshot');
    });
  }

  #handleNodeStatus(snapshot: NodeSnapshot, error: Error | null): void {
    if (snapshot.status === 'healthy') this.emit('nodeConnected', snapshot);
    else if (snapshot.status === 'disconnected' || snapshot.status === 'session_lost') {
      this.emit('nodeDisconnected', { node: snapshot, error });
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

function validateOptions(options: RemediaClientOptions): void {
  if (!options || !Array.isArray(options.nodes) || options.nodes.length === 0)
    throw new TypeError('nodes must contain at least one node');
  if (typeof options.send !== 'function')
    throw new TypeError('send must be a Discord Gateway callback');
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
  if (!/^\d{1,20}$/.test(value)) return false;
  try {
    return BigInt(value) > 0n && BigInt(value) <= 18_446_744_073_709_551_615n;
  } catch {
    return false;
  }
}
