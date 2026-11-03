export class RemediaClientError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}

export class RemediaRpcError extends RemediaClientError {
  readonly code: string;
  readonly nodeId: string;

  constructor(nodeId: string, code: string, message: string) {
    super(`Remedia node ${nodeId} returned ${code}: ${message}`);
    this.code = code;
    this.nodeId = nodeId;
  }
}

export class NodeUnavailableError extends RemediaClientError {
  readonly nodeId: string;

  constructor(nodeId: string, message = 'node is not connected') {
    super(`Remedia node ${nodeId} ${message}`);
    this.nodeId = nodeId;
  }
}

export class SessionLostError extends RemediaClientError {
  readonly nodeId: string;
  readonly guildIds: readonly string[];

  constructor(nodeId: string, guildIds: readonly string[], message: string) {
    super(`Remedia session on node ${nodeId} was lost: ${message}`);
    this.nodeId = nodeId;
    this.guildIds = guildIds;
  }
}

export class NodeSelectionError extends RemediaClientError {}

export class UnsupportedMediaSourceError extends RemediaClientError {}

export class ProtocolError extends RemediaClientError {
  readonly nodeId: string;

  constructor(nodeId: string, message: string) {
    super(`Remedia node ${nodeId} protocol error: ${message}`);
    this.nodeId = nodeId;
  }
}

export class DiscordBotMismatchError extends RemediaClientError {
  constructor(nodeId: string, expected: string, actual: string) {
    super(
      `Discord bot user mismatch on node ${nodeId}: expected ${expected}, connection identifies as ${actual}`,
    );
  }
}

export function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value));
}
