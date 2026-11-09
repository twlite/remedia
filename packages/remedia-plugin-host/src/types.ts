import type { Readable } from 'node:stream';

/** Cancellation scope. Handlers must settle promptly after cancellation. */
export interface OperationContext {
  /** Combined caller, registration and host cancellation. */
  readonly signal: AbortSignal;
}
/** Encoded bytes; object-mode Node streams are rejected during consumption. */
export type MediaStream = Readable | ReadableStream<Uint8Array>;
/** Lazy encoded source. Direct callers own opening and disposal. */
export interface MediaSource {
  /** Encoded response MIME type. */
  readonly contentType: string;
  /** Open a fresh stream; respect cancellation even while awaiting setup.
   * @param context Consumer lifetime.
   * @returns Encoded bytes, possibly asynchronously.
   */
  open(context: OperationContext): MediaStream | Promise<MediaStream>;
  /** Release factory resources, including when never opened. Called once per HTTP grant.
   * @returns Completion of cleanup; failures propagate to the consumer or error reporter.
   */
  dispose?(): void | Promise<void>;
}
/** Factory settings. */
export interface MediaSourceOptions extends Omit<MediaSource, 'contentType'> {
  /** MIME type; defaults to application/octet-stream. */
  contentType?: string;
}
/** Operation schema entry.
 * @typeParam Input In-process input type.
 * @typeParam Output Handler output type, excluding the unhandled sentinel undefined.
 */
export interface Operation<Input = unknown, Output = unknown> {
  /** Input contract; remote handlers must also validate untrusted input at runtime. */
  input: Input;
  /** Output contract. */
  output: Output;
}
/** Extend this interface using module augmentation for globally typed operations. */
export interface PluginOperations {}
/** Operation handler.
 * @typeParam I Input contract.
 * @typeParam O Output contract.
 * @param input Original input reference.
 * @param context Cancellation lifetime.
 * @returns Original output reference, or undefined to decline handling.
 */
export type Handler<I = unknown, O = unknown> = (
  input: I,
  context: OperationContext,
) => O | undefined | Promise<O | undefined>;
/** Registration schema.
 * @typeParam S Operation names mapped to Operation entries.
 */
export interface PluginDefinition<S = Record<string, Operation>> {
  /** Unique ID, 1–128 ASCII letters, digits, dots, underscores or hyphens; starts alphanumeric. */
  id: string;
  /** Own operation properties are copied at registration. */
  operations: { [K in keyof S]: S[K] extends Operation<infer I, infer O> ? Handler<I, O> : never };
  /** Release plugin resources after operations and HTTP streams settle.
   * @returns Cleanup completion. Rejections propagate through unregister/close.
   */
  dispose?(): void | Promise<void>;
}
/** Host configuration. No listener is started. */
export interface PluginHostOptions {
  /** Human-readable discovery name. */
  name: string;
  /** Trusted HTTP(S) origin for media URLs; required only for HTTP media results. */
  publicUrl?: string;
  /** Optional bearer secret, at least 16 characters. Discovery remains public. */
  token?: string;
  /** Token lifetime in milliseconds, 1–3600000; default 60000. */
  mediaTtlMs?: number;
  /** Maximum unredeemed tokens; default 1024. */
  maxMediaTokens?: number;
  /** Maximum request bytes; default 65536. */
  maxRequestBytes?: number;
  /** Report unexpected HTTP failures and background cleanup failures; defaults to console.error.
   * @param error Original failure, never included in HTTP error bodies.
   * @returns Nothing.
   */
  onError?: (error: unknown) => void;
}
/** Direct request; objects and streams are never serialized.
 * @typeParam I Input contract.
 */
export interface PluginRequest<I = unknown> {
  /** Registered plugin ID. */
  pluginId: string;
  /** Operation name. */
  operation: string;
  /** Original input reference. */
  input: I;
  /** Optional caller cancellation. */
  signal?: AbortSignal;
}
/** Additional cancellation, combined with the request signal. */
export interface ExecuteOptions {
  /** Transport or caller cancellation. */
  signal?: AbortSignal;
}
/** Dispatch outcome; null and false are handled values.
 * @typeParam T Original result type.
 */
export type ExecuteResult<T = unknown> =
  | {
      /** The operation declined handling. */
      handled: false;
    }
  | {
      /** The operation returned a value. */
      handled: true;
      /** Original successful result reference. */
      result: T;
    };
/** Stable protocol failure codes. */
export type ErrorCode =
  | 'host_closed'
  | 'plugin_unregistered'
  | 'duplicate_plugin'
  | 'invalid_request'
  | 'plugin_not_found'
  | 'operation_not_found'
  | 'cancelled'
  | 'method_not_allowed'
  | 'unauthorized'
  | 'request_too_large'
  | 'invalid_json'
  | 'unsupported_media_type'
  | 'invalid_result'
  | 'public_url_required'
  | 'media_token_limit'
  | 'media_not_found'
  | 'invalid_media_source'
  | 'plugin_failed'
  | 'not_found'
  | 'range_not_supported';
/** JSON-safe discovery metadata. */
export interface Manifest {
  /** Human-readable host name. */
  name: string;
  /** Wire protocol version. */
  protocolVersion: 1;
  /** Relative transport paths. */
  endpoints: {
    /** JSON operation execution route. */
    execute: string;
    /** Media route template with a token placeholder. */
    media: string;
  };
  /** Current registrations and their operation names. */
  plugins: {
    /** Registered plugin identifier. */
    id: string;
    /** Available application-defined operations. */
    operations: string[];
  }[];
}
