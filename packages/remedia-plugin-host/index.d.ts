import type { Readable } from 'node:stream';

/** JSON-safe values exchanged by the plugin-host protocol. */
export type JSONValue = null | boolean | number | string | JSONValue[] | { [key: string]: JSONValue };

/** A plugin operation receives a cancellation signal. */
export interface OperationContext {
  signal: AbortSignal;
}

/** Independently registered plugin operation handlers. */
export interface PluginDefinition {
  id: string;
  operations: Record<
    string,
    (input: JSONValue, context: OperationContext) =>
      | JSONValue
      | undefined
      | Readable
      | ReadableStream<Uint8Array>
      | Promise<JSONValue | undefined | Readable | ReadableStream<Uint8Array>>
  >;
  dispose?: () => void | Promise<void>;
}

/** Host identity, access token, and bounded media lifetime settings. */
export interface PluginHostOptions {
  name: string;
  publicUrl: string;
  token?: string;
  mediaTtlMs?: number;
  maxMediaTokens?: number;
}

/** Framework-independent plugin-host implementation. */
export declare class PluginHost {
  constructor(options: PluginHostOptions);
  register(plugin: PluginDefinition): void;
  unregister(id: string): Promise<boolean>;
  fetch(request: Request): Promise<Response>;
}

/** Construct a host whose operations are available through standard Fetch. */
export declare function createPluginHost(options: PluginHostOptions): PluginHost;
