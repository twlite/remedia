import { randomBytes, timingSafeEqual } from 'node:crypto';
import { Readable } from 'node:stream';
import { finished } from 'node:stream/promises';
import type {
  ErrorCode,
  ExecuteOptions,
  ExecuteResult,
  Handler,
  Manifest,
  MediaSource,
  MediaSourceOptions,
  MediaStream,
  Operation,
  PluginDefinition,
  PluginHostOptions,
  PluginOperations,
  PluginRequest,
} from './types.js';
export type * from './types.js';

const mediaPrefix = '/remedia/v1/media/';
const executePath = '/remedia/v1/execute';
const mediaSymbol = Symbol('remedia.media');
/** Typed host/protocol failure. Handler failures retain their original identity in direct calls. */
export class PluginHostError extends Error {
  /** Stable machine-readable code. */
  readonly code: ErrorCode;
  /** Corresponding HTTP status. */
  readonly status: number;
  /** Construct a protocol failure.
   * @param code Stable failure code.
   * @param status HTTP status, default 500.
   * @param options Optional original error cause.
   */
  constructor(code: ErrorCode, status = 500, options?: ErrorOptions) {
    super(code, options);
    this.name = 'PluginHostError';
    this.code = code;
    this.status = status;
  }
}
/** Create a deferred source; no stream is opened.
 * @param options Factory and optional cleanup/MIME type.
 * @returns Immutable source. HTTP owns each returned grant; direct callers own disposal.
 * @throws TypeError for an invalid factory or MIME type.
 */
export function createMediaSource(options: MediaSourceOptions): MediaSource {
  const contentType = options.contentType ?? 'application/octet-stream';
  if (typeof options.open !== 'function' || !contentType || /[\r\n]/.test(contentType))
    throw new TypeError('Invalid media source');
  return Object.freeze({ ...options, contentType, [mediaSymbol]: true });
}
/** Identify a branded lazy source.
 * @param value Candidate value.
 * @returns Whether createMediaSource produced the object.
 */
export function isMediaSource(value: unknown): value is MediaSource {
  return record(value) && mediaSymbol in value;
}
function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function validId(value: unknown): value is string {
  return typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}$/.test(value);
}
function isStream(value: unknown): value is MediaStream {
  return value instanceof Readable || value instanceof ReadableStream;
}
async function destroy(value: unknown) {
  if (value instanceof Readable) {
    const completion = finished(value, { cleanup: true });
    value.destroy();
    try {
      await completion;
    } catch (error) {
      if (
        !(error instanceof Error) ||
        !('code' in error) ||
        error.code !== 'ERR_STREAM_PREMATURE_CLOSE'
      )
        throw error;
    }
  } else if (value instanceof ReadableStream) await value.cancel();
}
function reply(value: unknown, status = 200) {
  return Response.json(value, { status, headers: { 'cache-control': 'no-store' } });
}
function fail(code: ErrorCode, status: number) {
  return reply({ error: { code } }, status);
}
// Reject lossy JSON conversion, including toJSON hooks, streams nested in objects and cycles.
function assertJSON(value: unknown, seen = new Set<object>()): void {
  if (
    value === null ||
    typeof value === 'string' ||
    typeof value === 'boolean' ||
    (typeof value === 'number' && Number.isFinite(value))
  )
    return;
  if (
    typeof value !== 'object' ||
    value === null ||
    seen.has(value) ||
    (!Array.isArray(value) &&
      Object.getPrototypeOf(value) !== Object.prototype &&
      Object.getPrototypeOf(value) !== null)
  )
    throw new PluginHostError('invalid_result', 502);
  seen.add(value);
  if (Object.getOwnPropertySymbols(value).length) throw new PluginHostError('invalid_result', 502);
  for (const key of Object.keys(value)) {
    const descriptor = Object.getOwnPropertyDescriptor(value, key)!;
    if (!('value' in descriptor)) throw new PluginHostError('invalid_result', 502);
    assertJSON(descriptor.value, seen);
  }
  if (Array.isArray(value) && Object.keys(value).length !== value.length)
    throw new PluginHostError('invalid_result', 502);
  seen.delete(value);
}
interface Registration {
  operations: Record<string, Handler>;
  controller: AbortController;
  tasks: Set<Promise<unknown>>;
  resources: Set<() => Promise<void>>;
  dispose?: () => void | Promise<void>;
}
interface Grant {
  plugin: Registration;
  source: MediaSource | MediaStream;
  expiresAt: number;
  timer: ReturnType<typeof setTimeout>;
  release: () => Promise<void>;
}
/** Framework-independent dispatcher and optional HTTP transport.
 * @typeParam S Operation schema, defaulting to the augmentable PluginOperations interface.
 */
export class PluginHost<S = PluginOperations> {
  #plugins = new Map<string, Registration>();
  #requests = new Set<Promise<Response>>();
  #removing = new Map<string, Promise<boolean>>();
  #media = new Map<string, Grant>();
  #controller = new AbortController();
  #closing?: Promise<void>;
  #options: PluginHostOptions;
  #origin?: string;
  /** Create a host without starting a server.
   * @param options Host and transport settings.
   * @throws TypeError or RangeError for invalid settings.
   */
  constructor(options: PluginHostOptions) {
    if (!options?.name?.trim()) throw new TypeError('name is required');
    if (options.publicUrl !== undefined) {
      const url = new URL(options.publicUrl);
      if (
        !['http:', 'https:'].includes(url.protocol) ||
        url.username ||
        url.password ||
        url.search ||
        url.hash ||
        url.pathname !== '/'
      )
        throw new TypeError('publicUrl must be an HTTP(S) origin');
      this.#origin = url.origin;
    }
    if (options.token !== undefined && options.token.length < 16)
      throw new TypeError('token must have at least 16 characters');
    for (const [name, value] of Object.entries({
      mediaTtlMs: options.mediaTtlMs ?? 60000,
      maxMediaTokens: options.maxMediaTokens ?? 1024,
      maxRequestBytes: options.maxRequestBytes ?? 65536,
    })) {
      if (!Number.isSafeInteger(value) || value < 1 || (name === 'mediaTtlMs' && value > 3600000))
        throw new RangeError(`Invalid ${name}`);
    }
    this.#options = { ...options };
  }
  #report(error: unknown) {
    (this.#options.onError ?? console.error)(error);
  }
  #check() {
    if (this.#controller.signal.aborted) throw new PluginHostError('host_closed', 503);
  }
  /** Register initialized operations. IDs cannot be reused until disposal settles.
   * @typeParam T Explicit operation schema for this registration.
   * @param plugin Plugin implementation; handlers must validate remote inputs at runtime.
   * @throws PluginHostError for duplicate IDs/closed host; TypeError for malformed definitions.
   */
  register<T = Record<string, Operation>>(plugin: PluginDefinition<NoInfer<T>>): void {
    this.#check();
    if (
      !validId(plugin?.id) ||
      !record(plugin.operations) ||
      !Object.keys(plugin.operations).length ||
      (plugin.dispose !== undefined && typeof plugin.dispose !== 'function')
    )
      throw new TypeError('Invalid plugin');
    if (this.#plugins.has(plugin.id) || this.#removing.has(plugin.id))
      throw new PluginHostError('duplicate_plugin', 409);
    const operations: Record<string, Handler> = Object.create(null);
    for (const [key, handler] of Object.entries(plugin.operations)) {
      if (!validId(key) || typeof handler !== 'function') throw new TypeError('Invalid operation');
      // The registration schema is erased only at this internal dispatcher boundary.
      operations[key] = handler as Handler;
    }
    this.#plugins.set(plugin.id, {
      operations,
      controller: new AbortController(),
      tasks: new Set(),
      resources: new Set(),
      ...(plugin.dispose ? { dispose: () => plugin.dispose!() } : {}),
    });
  }
  /** Dispatch without serialization. Successful direct results transfer ownership to the caller.
   * @typeParam K Operation key from the host schema.
   * @param request Original input and optional cancellation.
   * @param options Additional transport cancellation, combined with request cancellation.
   * @returns Discriminated outcome containing the original output reference.
   * @throws PluginHostError for validation/lookup; original handler/cancellation failures propagate.
   */
  handle<K extends keyof S & string>(
    request: PluginRequest<S[K] extends Operation<infer I, unknown> ? I : never> & { operation: K },
    options?: ExecuteOptions,
  ): Promise<ExecuteResult<S[K] extends Operation<unknown, infer O> ? O : never>>;
  /** Dispatch an untyped request when no operation schema is configured.
   * @param request Original input and optional cancellation.
   * @param options Additional transport cancellation.
   * @returns Outcome with the original unknown result.
   * @throws Validation, lookup, cancellation or original handler failures.
   */
  handle(
    request: keyof S extends never ? PluginRequest : never,
    options?: ExecuteOptions,
  ): Promise<ExecuteResult>;
  handle(request: PluginRequest, options: ExecuteOptions = {}): Promise<ExecuteResult> {
    return this.#dispatch(request, options);
  }
  async #dispatch(request: PluginRequest, options: ExecuteOptions = {}): Promise<ExecuteResult> {
    this.#check();
    if (
      !record(request) ||
      !Object.hasOwn(request, 'input') ||
      !validId(request.pluginId) ||
      !validId(request.operation)
    )
      throw new PluginHostError('invalid_request', 400);
    const plugin = this.#plugins.get(request.pluginId);
    if (!plugin) throw new PluginHostError('plugin_not_found', 404);
    const handler = plugin.operations[request.operation];
    if (!handler) throw new PluginHostError('operation_not_found', 404);
    const signal = AbortSignal.any([
      plugin.controller.signal,
      this.#controller.signal,
      ...[request.signal, options.signal].filter((s): s is AbortSignal => s !== undefined),
    ]);
    signal.throwIfAborted();
    const task = (async () => {
      const result = await handler(request.input, { signal });
      if (signal.aborted) {
        if (isMediaSource(result)) await result.dispose?.();
        else await destroy(result);
        throw signal.reason;
      }
      return result === undefined
        ? ({ handled: false } as const)
        : ({ handled: true, result } as const);
    })();
    plugin.tasks.add(task);
    try {
      return await task;
    } finally {
      plugin.tasks.delete(task);
    }
  }
  /** Positional convenience dispatcher.
   * @param pluginId Registered ID.
   * @param operation Operation name.
   * @param input Original input.
   * @param options Caller cancellation.
   * @returns Original dispatch outcome.
   */
  execute(
    pluginId: string,
    operation: string,
    input: unknown,
    options: ExecuteOptions = {},
  ): Promise<ExecuteResult> {
    return this.#dispatch({ pluginId, operation, input }, options);
  }
  /** Snapshot discovery metadata.
   * @returns JSON-safe current registrations.
   */
  manifest(): Manifest {
    return {
      name: this.#options.name,
      protocolVersion: 1,
      endpoints: { execute: executePath, media: mediaPrefix + '{token}' },
      plugins: [...this.#plugins].map(([id, plugin]) => ({
        id,
        operations: Object.keys(plugin.operations),
      })),
    };
  }
  /** Identify owned paths for middleware fallthrough.
   * @param path URL pathname.
   * @returns Whether the host owns the route.
   */
  matches(path: string): boolean {
    return path === '/.well-known/remedia' || path === executePath || path.startsWith(mediaPrefix);
  }
  /** Remove, cancel and drain a registration, then dispose exactly once.
   * @param id Registration ID.
   * @returns Whether a registration existed. Concurrent removals share completion.
   * @throws AggregateError if resource/plugin cleanup fails.
   */
  unregister(id: string): Promise<boolean> {
    const existing = this.#removing.get(id);
    if (existing) return existing;
    const plugin = this.#plugins.get(id);
    if (!plugin) return Promise.resolve(false);
    this.#plugins.delete(id);
    const task = Promise.resolve().then(async () => {
      plugin.controller.abort(new PluginHostError('plugin_unregistered', 503));
      await Promise.allSettled([...plugin.tasks]);
      const results = await Promise.allSettled([...plugin.resources].map((release) => release()));
      results.push(
        ...(await Promise.allSettled([Promise.resolve().then(() => plugin.dispose?.())])),
      );
      const failures = results.filter((r) => r.status === 'rejected');
      if (failures.length)
        throw new AggregateError(
          failures.map((r) => r.reason),
          'Plugin cleanup failed',
        );
      return true;
    });
    this.#removing.set(id, task);
    void task.finally(() => this.#removing.delete(id)).catch(() => {});
    return task;
  }
  /** Cancel all registrations and await cleanup. Repeated calls share the same promise.
   * @returns Shutdown completion; rejects with aggregated cleanup failures.
   */
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#controller.abort(new PluginHostError('host_closed', 503));
    this.#closing = (async () => {
      const removals = new Set(
        [...this.#plugins.keys()]
          .map((id) => this.unregister(id))
          .concat([...this.#removing.values()]),
      );
      const results = await Promise.allSettled([...removals, ...this.#requests]);
      const failures = results.filter((r) => r.status === 'rejected');
      if (failures.length)
        throw new AggregateError(
          failures.map((r) => r.reason),
          'Host cleanup failed',
        );
    })();
    return this.#closing;
  }
  /** Execute optional HTTP transport using the same dispatcher.
   * @param request Standard Fetch request; signal owns response streaming lifetime.
   * @returns Protocol response. Unexpected errors are reported and redacted.
   */
  async fetch(request: Request): Promise<Response> {
    const task = this.#fetch(request);
    this.#requests.add(task);
    try {
      return await task;
    } finally {
      this.#requests.delete(task);
    }
  }
  async #fetch(request: Request): Promise<Response> {
    const signal = AbortSignal.any([request.signal, this.#controller.signal]);
    try {
      this.#check();
      signal.throwIfAborted();
      const path = new URL(request.url).pathname;
      if (path === '/.well-known/remedia')
        return request.method === 'GET' ? reply(this.manifest()) : fail('method_not_allowed', 405);
      if (path === executePath) {
        if (request.method !== 'POST') return fail('method_not_allowed', 405);
        if (this.#options.token !== undefined) {
          const expected = Buffer.from('Bearer ' + this.#options.token);
          const actual = Buffer.from(request.headers.get('authorization') ?? '');
          if (expected.length !== actual.length || !timingSafeEqual(expected, actual))
            return fail('unauthorized', 401);
        }
        if (
          request.headers.get('content-type')?.split(';')[0]?.trim().toLowerCase() !==
          'application/json'
        )
          return fail('unsupported_media_type', 415);
        const body = await this.#read(request, signal);
        if (
          !record(body) ||
          !validId(body.pluginId) ||
          !validId(body.operation) ||
          !Object.hasOwn(body, 'input')
        )
          throw new PluginHostError('invalid_request', 400);
        const registration = this.#plugins.get(body.pluginId);
        const outcome = await this.#dispatch(
          { pluginId: body.pluginId, operation: body.operation, input: body.input },
          { signal },
        );
        if (!outcome.handled) return reply(outcome);
        if (isStream(outcome.result) || isMediaSource(outcome.result))
          return reply({
            handled: true,
            result: await this.#grant(String(body.pluginId), outcome.result, registration),
          });
        assertJSON(outcome.result);
        return reply(outcome);
      }
      if (path.startsWith(mediaPrefix)) {
        if (request.method !== 'GET') return fail('method_not_allowed', 405);
        if (request.headers.has('range')) return fail('range_not_supported', 416);
        const entry = this.#media.get(path.slice(mediaPrefix.length));
        if (!entry) return fail('media_not_found', 404);
        if (entry.expiresAt <= Date.now()) {
          await entry.release();
          return fail('media_not_found', 404);
        }
        this.#media.delete(path.slice(mediaPrefix.length));
        clearTimeout(entry.timer);
        return await this.#redeem(entry, signal);
      }
      return fail('not_found', 404);
    } catch (error) {
      if (error instanceof PluginHostError) return fail(error.code, error.status);
      if (signal.aborted) return fail('cancelled', 499);
      this.#report(error);
      return fail('plugin_failed', 502);
    }
  }
  async #read(request: Request, signal: AbortSignal): Promise<unknown> {
    const limit = this.#options.maxRequestBytes ?? 65536;
    if (Number(request.headers.get('content-length')) > limit) {
      await request.body?.cancel();
      throw new PluginHostError('request_too_large', 413);
    }
    const reader = request.body?.getReader();
    if (!reader) throw new PluginHostError('invalid_json', 400);
    const abort = () => {
      void reader.cancel(signal.reason).catch((e) => this.#report(e));
    };
    signal.addEventListener('abort', abort, { once: true });
    let size = 0;
    const chunks: Uint8Array[] = [];
    try {
      signal.throwIfAborted();
      while (true) {
        const { done, value } = await reader.read();
        signal.throwIfAborted();
        if (done) break;
        size += value.byteLength;
        if (size > limit) throw new PluginHostError('request_too_large', 413);
        chunks.push(value);
      }
      try {
        return JSON.parse(Buffer.concat(chunks, size).toString('utf8'));
      } catch (cause) {
        throw new PluginHostError('invalid_json', 400, { cause });
      }
    } catch (error) {
      await reader.cancel();
      throw error;
    } finally {
      signal.removeEventListener('abort', abort);
      reader.releaseLock();
    }
  }
  async #grant(id: string, source: MediaSource | MediaStream, expected: Registration | undefined) {
    const plugin = this.#plugins.get(id);
    const reject = async (code: ErrorCode, status: number): Promise<never> => {
      if (isMediaSource(source)) await source.dispose?.();
      else await destroy(source);
      throw new PluginHostError(code, status);
    };
    if (!plugin || plugin !== expected) return reject('plugin_unregistered', 503);
    if (!this.#origin) return reject('public_url_required', 500);
    if (this.#media.size >= (this.#options.maxMediaTokens ?? 1024))
      return reject('media_token_limit', 503);
    const token = randomBytes(32).toString('base64url');
    const ttl = this.#options.mediaTtlMs ?? 60000;
    let cleanup: Promise<void> | undefined;
    const release = () =>
      (cleanup ??= Promise.resolve().then(async () => {
        this.#media.delete(token);
        clearTimeout(entry.timer);
        try {
          if (isMediaSource(source)) await source.dispose?.();
          else await destroy(source);
        } finally {
          plugin.resources.delete(release);
        }
      }));
    const entry: Grant = {
      plugin,
      source,
      release,
      expiresAt: Date.now() + ttl,
      timer: setTimeout(() => {
        void release().catch((e) => this.#report(e));
      }, ttl),
    };
    entry.timer.unref();
    plugin.resources.add(release);
    this.#media.set(token, entry);
    // Observe eager Node stream failures while the token is waiting for redemption.
    if (source instanceof Readable)
      source.once('error', (error) => {
        this.#report(error);
        void release().catch((e) => this.#report(e));
      });
    return { url: this.#origin + mediaPrefix + token, expiresAtMs: entry.expiresAt };
  }
  async #redeem(entry: Grant, requestSignal: AbortSignal): Promise<Response> {
    const mediaController = new AbortController();
    const signal = AbortSignal.any([
      requestSignal,
      entry.plugin.controller.signal,
      mediaController.signal,
    ]);
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    let controller: ReadableStreamDefaultController<Uint8Array> | undefined;
    let finishing: Promise<void> | undefined;
    const finish = () =>
      (finishing ??= (async () => {
        signal.removeEventListener('abort', abort);
        try {
          try {
            await reader?.cancel();
          } finally {
            await entry.release();
          }
        } finally {
          entry.plugin.resources.delete(finish);
        }
      })());
    const abort = () => {
      controller?.error(signal.reason);
      void finish().catch((e) => this.#report(e));
    };
    const report = (error: unknown) => this.#report(error);
    const opening = (async () => {
      try {
        signal.throwIfAborted();
        const value = isMediaSource(entry.source)
          ? await entry.source.open({ signal })
          : entry.source;
        if (!isStream(value)) throw new PluginHostError('invalid_media_source', 502);
        if (signal.aborted) {
          await destroy(value);
          throw signal.reason;
        }
        reader = (
          value instanceof Readable
            ? (Readable.toWeb(value, {
                strategy: {
                  highWaterMark: 65536,
                  size: (chunk: unknown) => (chunk instanceof Uint8Array ? chunk.byteLength : 1),
                },
              }) as ReadableStream<Uint8Array>)
            : value
        ).getReader();
        entry.plugin.resources.delete(entry.release);
        entry.plugin.resources.add(finish);
        const body = new ReadableStream<Uint8Array>(
          {
            start(c) {
              controller = c;
            },
            async pull(c) {
              try {
                const item = await reader!.read();
                if (signal.aborted) return;
                if (item.done) {
                  await finish();
                  c.close();
                } else {
                  if (!(item.value instanceof Uint8Array))
                    throw new TypeError('Media must contain encoded bytes');
                  c.enqueue(item.value);
                }
              } catch (error) {
                report(error);
                c.error(error);
                try {
                  await finish();
                } catch (cleanup) {
                  report(
                    new AggregateError([error, cleanup], 'Stream cleanup failed', {
                      cause: cleanup,
                    }),
                  );
                }
              }
            },
            cancel: (reason) => {
              mediaController.abort(reason);
              return finish();
            },
          },
          { highWaterMark: 0 },
        );
        signal.addEventListener('abort', abort, { once: true });
        if (signal.aborted) abort();
        return new Response(body, {
          headers: {
            'content-type': isMediaSource(entry.source)
              ? entry.source.contentType
              : 'application/octet-stream',
            'cache-control': 'no-store',
          },
        });
      } catch (error) {
        await finish();
        throw error;
      }
    })();
    entry.plugin.tasks.add(opening);
    try {
      return await opening;
    } finally {
      entry.plugin.tasks.delete(opening);
    }
  }
}
/** Create a typed host without any HTTP listener.
 * @typeParam S Operation schema; alternatively augment PluginOperations.
 * @param options Host settings.
 * @returns Independent plugin host.
 */
export function createPluginHost<S = PluginOperations>(options: PluginHostOptions): PluginHost<S> {
  return new PluginHost<S>(options);
}
