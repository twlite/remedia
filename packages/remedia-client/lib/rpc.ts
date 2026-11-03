import { randomUUID } from 'node:crypto';
import { Buffer } from 'node:buffer';
import type { RawData, WebSocket } from 'ws';
import { NodeUnavailableError, ProtocolError, RemediaRpcError } from './errors.js';

export interface RpcLifecycleEvent {
  version: 1;
  event: string;
  sessionId: string;
  sequence: number;
  data: Record<string, unknown>;
}

interface RpcResponse {
  version: 1;
  id: string;
  ok: true;
  result: unknown;
}

interface PendingRequest {
  resolve(value: unknown): void;
  reject(reason: Error): void;
  timer: ReturnType<typeof setTimeout>;
  signal: AbortSignal | undefined;
  abort: (() => void) | undefined;
}

export interface RpcClientOptions {
  nodeId: string;
  timeoutMs: number;
  maxPending: number;
  onEvent(event: RpcLifecycleEvent): void;
  onMalformed(error: Error): void;
}

/** One socket's bounded JSON-RPC request correlator. */
export class RpcClient {
  readonly #socket: WebSocket;
  readonly #options: RpcClientOptions;
  readonly #pending = new Map<string, PendingRequest>();
  #counter = 0;
  #closed = false;

  constructor(socket: WebSocket, options: RpcClientOptions) {
    this.#socket = socket;
    this.#options = options;
    socket.on('message', (data: RawData, isBinary: boolean) => this.#receive(data, isBinary));
    socket.once('close', () => this.fail(new NodeUnavailableError(options.nodeId)));
    socket.once('error', () =>
      this.fail(new NodeUnavailableError(options.nodeId, 'connection failed')),
    );
  }

  get pendingCount(): number {
    return this.#pending.size;
  }

  request<Result>(
    command: string,
    params: Readonly<Record<string, unknown>> = {},
    options: { timeoutMs?: number; signal?: AbortSignal } = {},
  ): Promise<Result> {
    if (this.#closed || this.#socket.readyState !== this.#socket.OPEN) {
      return Promise.reject(new NodeUnavailableError(this.#options.nodeId));
    }
    if (this.#pending.size >= this.#options.maxPending) {
      return Promise.reject(
        new ProtocolError(this.#options.nodeId, 'pending request limit reached'),
      );
    }
    if (options.signal?.aborted) return Promise.reject(abortError());

    const id = `${randomUUID()}:${++this.#counter}`;
    const timeoutMs = options.timeoutMs ?? this.#options.timeoutMs;
    return new Promise<Result>((resolve, reject) => {
      const pending: PendingRequest = {
        resolve: (value) => resolve(value as Result),
        reject,
        timer: setTimeout(() => {
          this.#remove(id);
          reject(
            new RemediaRpcError(
              this.#options.nodeId,
              'request_timeout',
              `command ${command} timed out`,
            ),
          );
        }, timeoutMs),
        signal: options.signal,
        abort: undefined,
      };
      if (options.signal) {
        pending.abort = () => {
          this.#remove(id);
          reject(abortError());
        };
        options.signal.addEventListener('abort', pending.abort, { once: true });
      }
      this.#pending.set(id, pending);
      try {
        this.#socket.send(JSON.stringify({ version: 1, id, command, params }), (error) => {
          if (!error) return;
          this.#remove(id);
          reject(new NodeUnavailableError(this.#options.nodeId, 'could not send a request'));
        });
      } catch {
        this.#remove(id);
        reject(new NodeUnavailableError(this.#options.nodeId, 'could not send a request'));
      }
    });
  }

  fail(error: Error): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const [id, pending] of this.#pending) {
      this.#remove(id);
      pending.reject(error);
    }
  }

  #receive(data: RawData, isBinary: boolean): void {
    if (isBinary) {
      this.#options.onMalformed(
        new ProtocolError(this.#options.nodeId, 'binary RPC frames are unsupported'),
      );
      return;
    }
    let value: unknown;
    try {
      value = JSON.parse(
        typeof data === 'string' ? data : Buffer.concat(asBuffers(data)).toString('utf8'),
      );
    } catch {
      this.#options.onMalformed(new ProtocolError(this.#options.nodeId, 'received malformed JSON'));
      return;
    }
    if (!isRecord(value) || value.version !== 1) {
      this.#options.onMalformed(
        new ProtocolError(this.#options.nodeId, 'received an invalid protocol message'),
      );
      return;
    }
    if (typeof value.event === 'string') {
      if (
        typeof value.sessionId === 'string' &&
        Number.isSafeInteger(value.sequence) &&
        (value.sequence as number) >= 0 &&
        isRecord(value.data)
      ) {
        this.#options.onEvent(value as unknown as RpcLifecycleEvent);
      } else {
        this.#options.onMalformed(
          new ProtocolError(this.#options.nodeId, 'received a malformed lifecycle event'),
        );
      }
      return;
    }
    if (typeof value.id !== 'string' || typeof value.ok !== 'boolean') {
      this.#options.onMalformed(
        new ProtocolError(this.#options.nodeId, 'received a malformed RPC response'),
      );
      return;
    }
    const pending = this.#pending.get(value.id);
    if (!pending) return; // A timed out or cancelled request may still receive a late response.
    this.#remove(value.id);
    if (value.ok) {
      if (!('result' in value)) {
        pending.reject(new ProtocolError(this.#options.nodeId, 'success response has no result'));
      } else {
        pending.resolve((value as unknown as RpcResponse).result);
      }
    } else {
      if (
        !isRecord(value.error) ||
        typeof value.error.code !== 'string' ||
        typeof value.error.message !== 'string'
      ) {
        pending.reject(new ProtocolError(this.#options.nodeId, 'error response is malformed'));
      } else {
        pending.reject(
          new RemediaRpcError(this.#options.nodeId, value.error.code, value.error.message),
        );
      }
    }
  }

  #remove(id: string): void {
    const pending = this.#pending.get(id);
    if (!pending) return;
    clearTimeout(pending.timer);
    if (pending.signal && pending.abort) pending.signal.removeEventListener('abort', pending.abort);
    this.#pending.delete(id);
  }
}

function asBuffers(data: RawData): Buffer[] {
  if (Buffer.isBuffer(data)) return [data];
  if (Array.isArray(data)) return data;
  return [Buffer.from(data)];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function abortError(): Error {
  return new DOMException('The RPC request was cancelled', 'AbortError');
}
