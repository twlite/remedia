import type { IncomingMessage, ServerResponse } from 'node:http';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import type { PluginHost } from './index.js';
/** Connect/Express-compatible middleware. Mount at the origin root before body parsers.
 * @param host Fetch transport and route matcher.
 * @returns Middleware which falls through unknown paths and forwards unexpected errors to next.
 */
export function createNodeMiddleware(
  host: Pick<PluginHost, 'fetch' | 'matches'>,
): (req: IncomingMessage, res: ServerResponse, next?: (error?: unknown) => void) => Promise<void> {
  return async (req, res, next) => {
    const controller = new AbortController();
    const abort = () => {
      if (!res.writableFinished) controller.abort(new Error('client_disconnected'));
    };
    let response: Response | undefined;
    try {
      // The configured publicUrl, never an untrusted Host header, controls media descriptors.
      const url = new URL(req.url ?? '/', 'http://localhost');
      if (!host.matches(url.pathname)) {
        if (next) next();
        else {
          res.statusCode = 404;
          res.end();
        }
        return;
      }
      res.once('close', abort);
      req.once('aborted', abort);
      if ('body' in req || req.readableEnded) {
        res.statusCode = 400;
        res.end('Mount Remedia middleware before body parsers');
        return;
      }
      const headers = new Headers();
      for (const [key, value] of Object.entries(req.headers)) {
        if (Array.isArray(value)) for (const item of value) headers.append(key, item);
        else if (value !== undefined) headers.set(key, value);
      }
      const method = req.method ?? 'GET';
      const init: RequestInit & { duplex?: 'half' } = {
        method,
        headers,
        signal: controller.signal,
      };
      if (method !== 'GET' && method !== 'HEAD') {
        init.body = Readable.toWeb(req) as ReadableStream<Uint8Array>;
        init.duplex = 'half';
      }
      response = await host.fetch(new Request(url, init));
      if (controller.signal.aborted) {
        await response.body?.cancel();
        return;
      }
      res.statusCode = response.status;
      for (const [key, value] of response.headers) res.setHeader(key, value);
      if (!response.body || method === 'HEAD') {
        await response.body?.cancel();
        res.end();
        return;
      }
      await pipeline(
        Readable.fromWeb(response.body as import('node:stream/web').ReadableStream),
        res,
        { signal: controller.signal },
      );
    } catch (error) {
      if (controller.signal.aborted) return;
      if (next && !res.headersSent) next(error);
      else res.destroy(error instanceof Error ? error : new Error(String(error)));
    } finally {
      res.removeListener('close', abort);
      req.removeListener('aborted', abort);
    }
  };
}
