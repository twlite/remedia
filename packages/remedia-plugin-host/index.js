import { randomBytes } from 'node:crypto';
import { Readable } from 'node:stream';

const mediaPrefix = '/remedia/v1/media/';
const executePath = '/remedia/v1/execute';
const maxBody = 65536;
const mediaSymbol = Symbol('remedia.media');

function reply(value, status = 200) {
  return Response.json(value, { status, headers: { 'cache-control': 'no-store' } });
}

function fail(code, status) {
  return reply({ error: { code } }, status);
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function validId(value) {
  return typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}$/.test(value);
}

async function readJSON(request) {
  if (Number(request.headers.get('content-length')) > maxBody) throw new RangeError('request_too_large');
  const reader = request.body?.getReader();
  if (!reader) throw new SyntaxError('missing_body');
  let size = 0;
  const chunks = [];
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > maxBody) throw new RangeError('request_too_large');
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let cursor = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, cursor);
    cursor += chunk.byteLength;
  }
  return JSON.parse(new TextDecoder().decode(bytes));
}

/** Describe encoded media that should be opened only when consumed. */
export function createMediaSource({ open, contentType = 'application/octet-stream' }) {
  if (typeof open !== 'function') throw new TypeError('media source requires open()');
  return Object.freeze({ [mediaSymbol]: true, open, contentType });
}

/** Identify a deferred media source. */
export function isMediaSource(value) {
  return Boolean(value && typeof value === 'object' && value[mediaSymbol] === true);
}

/** Typed dispatch error for direct and HTTP callers. */
export class PluginHostError extends Error {
  constructor(code, status = 500) {
    super(code);
    this.name = 'PluginHostError';
    this.code = code;
    this.status = status;
  }
}

function isStream(value) {
  return value instanceof Readable || value instanceof ReadableStream;
}

async function destroyStream(value) {
  if (value instanceof Readable) value.destroy();
  else if (value instanceof ReadableStream) await value.cancel().catch(() => undefined);
}

/** Create a framework-independent Remedia HTTP plugin host.
 * @param {{name: string, publicUrl: string, token?: string, mediaTtlMs?: number, maxMediaTokens?: number}} options Host settings.
 * @returns {PluginHost} A Fetch-compatible host.
 */
export function createPluginHost(options) {
  return new PluginHost(options);
}

/** Stateful registrations exposed through independent HTTP requests. */
export class PluginHost {
  #plugins = new Map();
  #media = new Map();
  #name;
  #origin;
  #token;
  #ttl;
  #maxTokens;
  #closed = false;

  /** @param {{name: string, publicUrl: string, token?: string, mediaTtlMs?: number, maxMediaTokens?: number}} options Host settings. */
  constructor(options) {
    if (!options || typeof options.name !== 'string' || !options.name.trim()) throw new TypeError('name is required');
    let url;
    if (options.publicUrl !== undefined) {
      url = new URL(options.publicUrl);
      if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== '/') {
        throw new TypeError('publicUrl must be an HTTP(S) origin without credentials');
      }
    }
    if (options.token !== undefined && (typeof options.token !== 'string' || options.token.length < 16)) {
      throw new TypeError('token must have at least 16 characters');
    }
    if (options.mediaTtlMs !== undefined && (!Number.isSafeInteger(options.mediaTtlMs) || options.mediaTtlMs < 1000 || options.mediaTtlMs > 3600000)) {
      throw new RangeError('mediaTtlMs must be between 1000 and 3600000');
    }
    if (options.maxMediaTokens !== undefined && (!Number.isSafeInteger(options.maxMediaTokens) || options.maxMediaTokens < 1)) {
      throw new RangeError('maxMediaTokens must be positive');
    }
    this.#name = options.name;
    this.#origin = url?.origin;
    this.#token = options.token;
    this.#ttl = options.mediaTtlMs ?? 60000;
    this.#maxTokens = options.maxMediaTokens ?? 1024;
  }

  /** Register independently initialized plugin operations.
   * @param {{id: string, operations: Record<string, Function>, dispose?: () => Promise<void> | void}} plugin Plugin implementation.
   */
  register(plugin) {
    if (!validId(plugin?.id) || !isRecord(plugin.operations) || !Object.keys(plugin.operations).length) {
      throw new TypeError('plugin requires id and operations');
    }
    if (this.#closed) throw new PluginHostError('host_closed', 503);
    if (this.#plugins.has(plugin.id)) throw new Error('Duplicate plugin ID: ' + plugin.id);
    for (const [operation, handler] of Object.entries(plugin.operations)) {
      if (!validId(operation) || typeof handler !== 'function') throw new TypeError('invalid operation handler');
    }
    this.#plugins.set(plugin.id, { ...plugin, controller: new AbortController() });
  }

  /** Handle an operation request directly, without any HTTP connection or serialization.
   * @param {{pluginId: string, operation: string, input: unknown, signal?: AbortSignal}} request Structured operation request.
   * @param {{signal?: AbortSignal}} options Optional transport-specific cancellation override.
   * @returns {Promise<{handled: boolean, result?: unknown}>} Original handler result, unmodified.
   */
  async handle(request, options = {}) {
    if (this.#closed) throw new PluginHostError('host_closed', 503);
    if (!isRecord(request) || !Object.hasOwn(request, 'input') ||
        !validId(request.pluginId) || !validId(request.operation)) {
      throw new PluginHostError('invalid_request', 400);
    }
    const { pluginId, operation, input } = request;
    const plugin = this.#plugins.get(pluginId);
    if (!plugin) throw new PluginHostError('plugin_not_found', 404);
    const handler = Object.hasOwn(plugin.operations, operation) ? plugin.operations[operation] : undefined;
    if (typeof handler !== 'function') throw new PluginHostError('operation_not_found', 404);
    const incomingSignal = options.signal ?? request.signal;
    const signal = incomingSignal
      ? AbortSignal.any([incomingSignal, plugin.controller.signal])
      : plugin.controller.signal;
    signal.throwIfAborted();
    const result = await handler(input, { signal });
    if (signal.aborted) {
      if (isStream(result)) await destroyStream(result);
      throw signal.reason;
    }
    return result === undefined ? { handled: false } : { handled: true, result };
  }

  /** Convenience overload for direct operation dispatch.
   * @param {string} pluginId Registered plugin ID.
   * @param {string} operation Operation name.
   * @param {unknown} input Handler input without serialization.
   * @param {{signal?: AbortSignal}} options Optional cancellation.
   * @returns {Promise<{handled: boolean, result?: unknown}>} Raw dispatch result.
   */
  async execute(pluginId, operation, input, options = {}) {
    return this.handle({ pluginId, operation, input, signal: options.signal });
  }

  /** Obtain JSON-safe host discovery metadata without making an HTTP request. */
  manifest() {
    return {
      name: this.#name,
      protocolVersion: 1,
      endpoints: { execute: executePath, media: mediaPrefix + '{token}' },
      plugins: [...this.#plugins.values()].map((plugin) => ({
        id: plugin.id,
        operations: Object.keys(plugin.operations),
      })),
    };
  }

  /** Check whether an HTTP path belongs to the plugin host. */
  matches(path) {
    return path === '/.well-known/remedia' || path === executePath || path.startsWith(mediaPrefix);
  }

  /** Close the host and dispose all registrations. */
  async close() {
    if (this.#closed) return;
    this.#closed = true;
    const results = await Promise.allSettled([...this.#plugins.keys()].map((id) => this.unregister(id)));
    const failures = results.filter((item) => item.status === 'rejected');
    if (failures.length) throw new AggregateError(failures.map((item) => item.reason), 'plugin cleanup failed');
  }

  /** Remove a registration and any unclaimed media it owns.
   * @param {string} id Plugin identifier.
   * @returns {Promise<boolean>} Whether it was registered.
   */
  async unregister(id) {
    const plugin = this.#plugins.get(id);
    if (!plugin) return false;
    this.#plugins.delete(id);
    plugin.controller.abort(new Error('plugin_unregistered'));
    for (const [token, entry] of this.#media) {
      if (entry.pluginId !== id) continue;
      clearTimeout(entry.timer);
      this.#media.delete(token);
      await destroyStream(entry.stream);
    }
    await plugin.dispose?.();
    return true;
  }

  /** Execute the protocol using the standard Fetch API.
   * @param {Request} request HTTP request.
   * @returns {Promise<Response>} HTTP response.
   */
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (this.#closed) return fail('host_closed', 503);
    if (path === '/.well-known/remedia') {
      if (request.method !== 'GET') return fail('method_not_allowed', 405);
      return reply(this.manifest());
    }
    if (path === executePath) {
      if (request.method !== 'POST') return fail('method_not_allowed', 405);
      if (this.#token !== undefined && request.headers.get('authorization') !== 'Bearer ' + this.#token) {
        return fail('unauthorized', 401);
      }
      let body;
      try {
        body = await readJSON(request);
      } catch (cause) {
        return fail(cause instanceof RangeError ? 'request_too_large' : 'invalid_json', cause instanceof RangeError ? 413 : 400);
      }
      if (!isRecord(body) || !validId(body.pluginId) || !validId(body.operation) || !Object.hasOwn(body, 'input')) {
        return fail('invalid_request', 400);
      }
      try {
        const outcome = await this.handle(body, { signal: request.signal });
        if (!outcome.handled) return reply({ handled: false });
        if (isStream(outcome.result) || isMediaSource(outcome.result)) {
          return reply({ handled: true, result: this.#grant(body.pluginId, outcome.result) });
        }
        return reply({ handled: true, result: outcome.result });
      } catch (cause) {
        if (cause instanceof PluginHostError) return fail(cause.code, cause.status);
        return fail(request.signal.aborted ? 'cancelled' : 'plugin_failed', request.signal.aborted ? 499 : 502);
      }
    }
    if (path.startsWith(mediaPrefix)) {
      if (request.method !== 'GET') return fail('method_not_allowed', 405);
      const token = path.slice(mediaPrefix.length);
      const entry = this.#media.get(token);
      if (!entry || entry.expiresAt <= Date.now()) {
        if (entry) {
          clearTimeout(entry.timer);
          this.#media.delete(token);
          await destroyStream(entry.stream);
        }
        return fail('media_not_found', 404);
      }
      this.#media.delete(token);
      clearTimeout(entry.timer);
      let value;
      try {
        value = isMediaSource(entry.stream) ? await entry.stream.open({ signal: request.signal }) : entry.stream;
        if (!isStream(value)) return fail('invalid_media_source', 502);
      } catch {
        return fail('plugin_failed', 502);
      }
      const stream = value instanceof Readable ? Readable.toWeb(value) : value;
      request.signal.addEventListener('abort', () => {
        void stream.cancel().catch(() => undefined);
      }, { once: true });
      return new Response(stream, {
        headers: { 'content-type': entry.stream.contentType ?? 'application/octet-stream', 'cache-control': 'no-store' },
      });
    }
    return fail('not_found', 404);
  }

  #grant(pluginId, stream) {
    if (!this.#origin) {
      if (isStream(stream)) void destroyStream(stream);
      throw new PluginHostError('public_url_required', 500);
    }
    if (this.#media.size >= this.#maxTokens) {
      void destroyStream(stream);
      throw new RangeError('media_token_limit');
    }
    const token = randomBytes(32).toString('base64url');
    const expiresAt = Date.now() + this.#ttl;
    const timer = setTimeout(() => {
      const item = this.#media.get(token);
      if (!item) return;
      this.#media.delete(token);
      void destroyStream(item.stream);
    }, this.#ttl);
    timer.unref?.();
    this.#media.set(token, { pluginId, stream, expiresAt, timer });
    return { url: this.#origin + mediaPrefix + token, expiresAtMs: expiresAt };
  }
}
