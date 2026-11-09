import type { Readable } from 'node:stream';

/** JSON-compatible value for the remote HTTP protocol. Local calls may pass richer objects. */
export type JSONValue = null | boolean | number | string | JSONValue[] | { [key: string]: JSONValue };

/** Cancellation scope for an operation or deferred media producer. */
export interface OperationContext {
  signal: AbortSignal;
}

/** Encoded media is opened only after its one-use URL is redeemed. */
export interface MediaSource {
  readonly contentType: string;
  open(context: OperationContext): Readable | ReadableStream<Uint8Array> | Promise<Readable | ReadableStream<Uint8Array>>;
}

/** Create a lazily opened media source that remains a normal object for local callers. */
export declare function createMediaSource(options: {
  open: MediaSource['open'];
  contentType?: string;
}): MediaSource;

/** Test whether a value was produced by createMediaSource. */
export declare function isMediaSource(value: unknown): value is MediaSource;

/** Generic handler result. Local invocation retains object identity and does not serialize values. */
export type OperationResult = unknown;

/** Remedia imposes no provider-specific operation names or semantics. */
export interface PluginDefinition {
  id: string;
  operations: Record<string, (input: any, context: OperationContext) => OperationResult | Promise<OperationResult>>;
  dispose?: () => void | Promise<void>;
}

/** Optional remote-transport settings; publicUrl is unnecessary for local-only use. */
export interface PluginHostOptions {
  name: string;
  publicUrl?: string;
  token?: string;
  mediaTtlMs?: number;
  maxMediaTokens?: number;
}

/** Outcome of direct dispatch. Undefined indicates an unhandled operation. */
export type ExecuteResult<T = unknown> = { handled: false } | { handled: true; result: T };

export declare class PluginHostError extends Error {
  constructor(code: string, status?: number);
  readonly code: string;
  readonly status: number;
}

/** Framework-independent operation registry and optional HTTP protocol handler. */
/** Structured in-process operation message. No HTTP Request is involved. */
export interface PluginRequest<Input = unknown> {
  pluginId: string;
  operation: string;
  input: Input;
  signal?: AbortSignal;
}

export declare class PluginHost {
  constructor(options: PluginHostOptions);
  register(plugin: PluginDefinition): void;
  unregister(id: string): Promise<boolean>;
  handle<T = unknown>(request: PluginRequest, options?: { signal?: AbortSignal }): Promise<ExecuteResult<T>>;
  execute<T = unknown>(pluginId: string, operation: string, input: unknown, options?: { signal?: AbortSignal }): Promise<ExecuteResult<T>>;
  manifest(): {
    name: string;
    protocolVersion: number;
    endpoints: { execute: string; media: string };
    plugins: { id: string; operations: string[] }[];
  };
  matches(path: string): boolean;
  fetch(request: Request): Promise<Response>;
  close(): Promise<void>;
}

/** Create a host without starting an HTTP server or loading provider-specific implementations. */
export declare function createPluginHost(options: PluginHostOptions): PluginHost;
