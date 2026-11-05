/** Base class for errors raised by the hybrid Remedia client. */
export class RemediaClientError extends Error {
  /**
   * Create a RemediaClientError with its failure context.
   * @param message - Human-readable explanation of the failure.
   * @param options - Standard Error options, including an optional underlying cause.
   */
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}

/** Structured failure response returned by a remote node RPC command. */
export class RemediaRpcError extends RemediaClientError {
  /** Machine-readable error code supplied by the remote server. */
  readonly code: string;
  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  readonly nodeId: string;

  /**
   * Create a RemediaRpcError with its failure context.
   * @param nodeId - Application-configured identifier of the affected Remedia node.
   * @param code - Machine-readable error code returned by the remote server.
   * @param message - Human-readable explanation of the failure.
   */
  constructor(nodeId: string, code: string, message: string) {
    super(`Remedia node ${nodeId} returned ${code}: ${message}`);
    this.code = code;
    this.nodeId = nodeId;
  }
}

/** The selected node cannot accept requests because its connection is unavailable. */
export class NodeUnavailableError extends RemediaClientError {
  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  readonly nodeId: string;

  /**
   * Create a NodeUnavailableError with its failure context.
   * @param nodeId - Application-configured identifier of the affected Remedia node.
   * @param message - Human-readable explanation of the failure.
   */
  constructor(nodeId: string, message = 'node is not connected') {
    super(`Remedia node ${nodeId} ${message}`);
    this.nodeId = nodeId;
  }
}

/** The server no longer owns a resumable client session and its affected guild assignments. */
export class SessionLostError extends RemediaClientError {
  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  readonly nodeId: string;
  /** Guild snowflakes affected by the lost or resumed remote client session. */
  readonly guildIds: readonly string[];

  /**
   * Create a SessionLostError with its failure context.
   * @param nodeId - Application-configured identifier of the affected Remedia node.
   * @param guildIds - Guild snowflakes affected by the lost remote client session.
   * @param message - Human-readable explanation of the failure.
   */
  constructor(nodeId: string, guildIds: readonly string[], message: string) {
    super(`Remedia session on node ${nodeId} was lost: ${message}`);
    this.nodeId = nodeId;
    this.guildIds = guildIds;
  }
}

/** No suitable node could be selected, or the selected assignment is unavailable. */
export class NodeSelectionError extends RemediaClientError {}

/** The chosen node cannot accept the requested media input type. */
export class UnsupportedMediaSourceError extends RemediaClientError {}

/** A remote connection violated the supported WebSocket RPC contract. */
export class ProtocolError extends RemediaClientError {
  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  readonly nodeId: string;

  /**
   * Create a ProtocolError with its failure context.
   * @param nodeId - Application-configured identifier of the affected Remedia node.
   * @param message - Human-readable explanation of the failure.
   */
  constructor(nodeId: string, message: string) {
    super(`Remedia node ${nodeId} protocol error: ${message}`);
    this.nodeId = nodeId;
  }
}

/** The requested bot user differs from the bot identity bound to the client or remote connection. */
export class DiscordBotMismatchError extends RemediaClientError {
  /**
   * Create a DiscordBotMismatchError with its failure context.
   * @param nodeId - Application-configured identifier of the affected Remedia node.
   * @param expected - Discord bot user snowflake required by the session.
   * @param actual - Discord bot user snowflake bound to the client or remote connection.
   */
  constructor(nodeId: string, expected: string, actual: string) {
    super(
      `Discord bot user mismatch on node ${nodeId}: expected ${expected}, connection identifies as ${actual}`,
    );
  }
}

/**
 * Preserve an Error instance or convert another thrown value to an Error.
 * @param value - Thrown value to normalize as an Error instance.
 * @returns An Error instance describing the supplied value.
 */
export function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value));
}
