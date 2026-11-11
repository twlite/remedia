import type { NodeAdapter } from './adapters.js';
import { NodeSelectionError, PluginOperationsUnsupportedError } from './errors.js';
import { RemoteNodeAdapter } from './remote-node.js';
import type {
  GuildAssignment,
  JsonValue,
  PluginExecuteOptions,
  PluginExecuteResult,
  PluginHostDescriptor,
  PluginListOptions,
} from './types.js';

/** Client state the plugin API needs. It deliberately exposes no way to create assignments or sessions. @internal */
export interface PluginOperationsRuntime {
  /** Throws when the owning client is closing or closed. */
  assertOpen(): void;
  /** Configured nodes by application-assigned ID. */
  nodes(): ReadonlyMap<string, NodeAdapter>;
  /** Sticky assignment of a guild, if one exists. */
  assignment(guildId: string): GuildAssignment | undefined;
}

/**
 * Discovery and execution of generic plugin-host operations through Remedia nodes.
 *
 * Applications configure only their Remedia nodes. The server operator decides which
 * `(pluginId, operation)` pairs may run, and this API discovers them automatically. Remedia
 * forwards opaque JSON and never interprets it: operation semantics belong to the application.
 *
 * Nothing here creates guild sessions, assignments or migrations, and nothing falls back to
 * another host or node.
 */
export class PluginOperations {
  readonly #runtime: PluginOperationsRuntime;

  /**
   * Create the plugin API for one client.
   * @param runtime - Read-only access to the client's node pool and sticky assignments.
   * @internal
   */
  constructor(runtime: PluginOperationsRuntime) {
    this.#runtime = runtime;
  }

  /**
   * Discover plugin hosts and the operations the server permits on them.
   *
   * With `nodeId` (or the sticky node of `guildId`) exactly that node is queried. Otherwise
   * every healthy remote node which forwards operations is queried and the results are combined;
   * embedded nodes, unhealthy nodes, and servers without forwarding support are skipped because
   * they cannot execute operations. A node failing mid-request rejects the call.
   * @param options - Node selection, refresh, cancellation and deadline.
   * @returns Hosts sorted by `nodeId`, then `hostId`. Each carries its owning node for affinity.
   * @throws {PluginOperationsUnsupportedError} If an explicitly selected node cannot forward operations.
   * @throws {NodeSelectionError} If the selection is contradictory or names an unknown node.
   * @throws {RemediaRpcError} If a node rejects the request.
   * @throws {DOMException} An `AbortError` if `options.signal` aborts.
   */
  async list(options: PluginListOptions = {}): Promise<readonly PluginHostDescriptor[]> {
    this.#runtime.assertOpen();
    throwIfAborted(options.signal);
    validateTimeout(options.timeoutMs);

    const rpc = {
      refresh: options.refresh === true,
      ...(options.signal ? { signal: options.signal } : {}),
      ...(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs }),
    };
    const pinned = this.#pinnedNode(options.nodeId, options.guildId);
    const nodes = pinned
      ? [this.#forwardingNode(pinned)]
      : [...this.#runtime.nodes().values()]
          .filter(
            (node): node is RemoteNodeAdapter =>
              node instanceof RemoteNodeAdapter &&
              node.status === 'healthy' &&
              node.snapshot.capabilities?.pluginOperations.forwarding === true,
          )
          .sort((left, right) => compare(left.id, right.id));
    const hosts = (await Promise.all(nodes.map((node) => node.listPluginHosts(rpc)))).flat();

    return hosts.sort(
      (left, right) => compare(left.nodeId, right.nodeId) || compare(left.hostId, right.hostId),
    );
  }

  /**
   * Execute one allowlisted operation on one plugin host.
   *
   * The node is chosen deterministically: an explicit `nodeId`, else the sticky node of `guildId`,
   * else the only healthy node which advertises `hostId`. Several candidates are ambiguous and
   * rejected, because hosts of the same ID on different nodes may be different deployments.
   * @param options - Target host, operation, JSON input, node selection, cancellation and deadline.
   * @typeParam Result - Expected JSON result type. Remedia passes the result through without validating it.
   * @returns A discriminated outcome: `handled: true` with the result (any JSON value) or `handled: false`.
   *   Both carry the executing `nodeId` so related operation sources can be played on the same node.
   * @throws {TypeError} If an identifier is empty or the input is not JSON-serializable.
   * @throws {NodeSelectionError} If no node, several nodes, or a contradictory node is selected.
   * @throws {PluginOperationsUnsupportedError} If the selected node is embedded or cannot forward operations.
   * @throws {RemediaRpcError} With the server's code, e.g. `plugin_operation_not_allowed`,
   *   `plugin_host_unavailable`, `plugin_host_unauthorized` or `request_timeout`.
   * @throws {DOMException} An `AbortError` if `options.signal` aborts; the server cancels the work.
   */
  async execute<Result extends JsonValue = JsonValue>(
    options: PluginExecuteOptions,
  ): Promise<PluginExecuteResult<Result>> {
    this.#runtime.assertOpen();
    throwIfAborted(options.signal);

    for (const field of ['hostId', 'pluginId', 'operation'] as const) {
      if (typeof options[field] !== 'string' || options[field].length === 0)
        throw new TypeError(`${field} must be a non-empty string`);
    }

    assertJson(options.input);
    validateTimeout(options.timeoutMs);

    const node = this.#forwardingNode(
      this.#pinnedNode(options.nodeId, options.guildId) ?? this.#nodeForHost(options.hostId),
    );

    return (await node.executePluginOperation(
      {
        hostId: options.hostId,
        pluginId: options.pluginId,
        operation: options.operation,
        input: options.input,
      },
      {
        ...(options.signal ? { signal: options.signal } : {}),
        ...(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs }),
      },
    )) as PluginExecuteResult<Result>;
  }

  /** The node named explicitly or through a sticky assignment, or undefined when neither is given. */
  #pinnedNode(nodeId: string | undefined, guildId: string | undefined): NodeAdapter | undefined {
    const assignment = guildId === undefined ? undefined : this.#runtime.assignment(guildId);

    if (assignment && nodeId !== undefined && assignment.nodeId !== nodeId) {
      throw new NodeSelectionError(
        `guild ${guildId} is assigned to node ${assignment.nodeId}; refusing to run on node ${nodeId}`,
      );
    }

    if (assignment?.status === 'lost') {
      throw new NodeSelectionError(
        `guild ${guildId} remains assigned to lost node ${assignment.nodeId}`,
      );
    }

    const selected = nodeId ?? assignment?.nodeId;

    if (selected === undefined) return undefined;

    const node = this.#runtime.nodes().get(selected);

    if (!node) throw new NodeSelectionError(`unknown Remedia node ${selected}`);

    return node;
  }

  /** The single healthy remote node advertising a host; anything else is not safe to choose. */
  #nodeForHost(hostId: string): NodeAdapter {
    const candidates = [...this.#runtime.nodes().values()]
      .filter(
        (node): node is RemoteNodeAdapter =>
          node instanceof RemoteNodeAdapter &&
          node.status === 'healthy' &&
          node.snapshot.capabilities?.pluginOperations.forwarding === true &&
          node.snapshot.capabilities.pluginOperations.hosts.includes(hostId),
      )
      .sort((left, right) => compare(left.id, right.id));

    if (candidates.length === 0) {
      throw new NodeSelectionError(
        `no healthy Remedia node forwards plugin operations for host ${hostId}`,
      );
    }

    if (candidates.length > 1) {
      throw new NodeSelectionError(
        `host ${hostId} is configured on several nodes (${candidates.map((node) => node.id).join(', ')}); pass nodeId to choose one`,
      );
    }

    return candidates[0]!;
  }

  #forwardingNode(node: NodeAdapter): RemoteNodeAdapter {
    if (!(node instanceof RemoteNodeAdapter)) {
      throw new PluginOperationsUnsupportedError(
        node.id,
        'embedded nodes run in-process and have no plugin-host registry; register plugins with the embedded engine instead',
      );
    }

    return node;
  }
}

function compare(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) throw new DOMException('The plugin request was cancelled', 'AbortError');
}

function validateTimeout(timeoutMs: number | undefined): void {
  if (timeoutMs !== undefined && (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1))
    throw new RangeError('timeoutMs must be a positive safe integer');
}

function assertJson(input: unknown): void {
  let encoded: string | undefined;

  try {
    encoded = JSON.stringify(input);
  } catch (cause) {
    throw new TypeError('input must be JSON-serializable', { cause });
  }

  if (encoded === undefined) throw new TypeError('input must be a JSON value');
}
