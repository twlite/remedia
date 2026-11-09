import { randomBytes } from 'node:crypto';
import { Readable } from 'node:stream';

const mediaPrefix = '/remedia/v1/media/';
const executePath = '/remedia/v1/execute';
const maxBody = 65536;

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
  return typeof value === 'string' && /^[a-zA-Z0-9][\\w.-]{0,127}$/.test(value);
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

  /** @param {{name: string, publicUrl: string, token?: string, mediaTtlMs?: number, maxMediaTokens?: number}} options Host settings. */
  constructor(options) {
    if (!options || typeof options.name !== 'string' || !options.name.trim()) throw new TypeError('name is required');
    const url = new URL(options.publicUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== '/') {
      throw new TypeError('publicUrl must be an HTTP(S) origin without credentials');
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
    this.#origin = url.origin;
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
    if (this.#plugins.has(plugin.id)) throw new Error('Duplicate plugin ID: ' + plugin.id);
    for (const [operation, handler] of Object.entries(plugin.operations)) {
      if (!validId(operation) || typeof handler !== 'function') throw new TypeError('invalid operation handler');
    }
    this.#plugins.set(plugin.id, plugin);
  }

  /** Remove a registration and any unclaimed media it owns.
   * @param {string} id Plugin identifier.
   * @returns {Promise<boolean>} Whether it was registered.
   */
  async unregister(id) {
    const plugin = this.#plugins.get(id);
    if (!plugin) return false;
    this.#plugins.delete(id);
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
    if (path === '/.well-known/remedia') {
      if (request.method !== 'GET') return fail('method_not_allowed', 405);
      return reply({
        name: this.#name,
        protocolVersion: 1,
        endpoints: { execute: executePath, media: mediaPrefix + '{token}' },
        plugins: [...this.#plugins.values()].map((plugin) => ({
          id: plugin.id,
          operations: Object.keys(plugin.operations),
        })),
      });
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
      const plugin = this.#plugins.get(body.pluginId);
      if (!plugin) return fail('plugin_not_found', 404);
      const handler = Object.hasOwn(plugin.operations, body.operation) ? plugin.operations[body.operation] : undefined;
      if (!handler) return fail('operation_not_found', 404);
      try {
        const result = await handler(body.input, { signal: request.signal });
        if (request.signal.aborted) {
          if (isStream(result)) await destroyStream(result);
          return fail('cancelled', 499);
        }
        if (isStream(result)) {
          if (body.operation !== 'track.stream') {
            await destroyStream(result);
            return fail('invalid_operation_result', 502);
          }
          return reply({ handled: true, result: this.#grant(body.pluginId, result) });
        }
        if (result === undefined) return reply({ handled: false });
        return reply({ handled: true, result });
      } catch {
        return fail('plugin_failed', 502);
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
      const stream = entry.stream instanceof Readable ? Readable.toWeb(entry.stream) : entry.stream;
      request.signal.addEventListener('abort', () => {
        void stream.cancel().catch(() => undefined);
      }, { once: true });
      return new Response(stream, {
        headers: { 'content-type': 'application/octet-stream', 'cache-control': 'no-store' },
      });
    }
    return fail('not_found', 404);
  }

  #grant(pluginId, stream) {
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
