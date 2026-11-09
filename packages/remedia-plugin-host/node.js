import { Readable } from 'node:stream';

/**
 * Adapt an HTTP Fetch handler to Connect/Express and Node HTTP.
 * Unknown paths fall through to the next middleware where available.
 * Mount before body parsers to preserve incoming request streams.
 * @param {{fetch(request: Request): Promise<Response>, matches?(path: string): boolean}} host Plugin host.
 * @returns {(req: import('node:http').IncomingMessage, res: import('node:http').ServerResponse, next?: (error?: unknown) => void) => Promise<void>} Middleware.
 */
export function createNodeMiddleware(host) {
  return async (req, res, next) => {
    const path = new URL(req.url || '/', 'http://localhost').pathname;
    if (typeof host.matches === 'function' && !host.matches(path)) {
      if (next) next();
      else {
        res.statusCode = 404;
        res.end();
      }
      return;
    }

    const controller = new AbortController();
    // IncomingMessage 'close' fires after the request body has been read, not
    // necessarily when the client disconnects. Watch the response instead.
    const abort = () => {
      if (!res.writableFinished) controller.abort(new Error('client_disconnected'));
    };
    res.once('close', abort);
    let source;
    try {
      const method = req.method || 'GET';
      const url = new URL(req.url || '/', 'http://' + (req.headers.host || 'localhost'));
      const request = new Request(url, {
        method,
        headers: req.headers,
        body: method === 'GET' || method === 'HEAD' ? undefined : Readable.toWeb(req),
        ...(method === 'GET' || method === 'HEAD' ? {} : { duplex: 'half' }),
        signal: controller.signal,
      });
      const response = await host.fetch(request);
      res.statusCode = response.status;
      for (const [key, value] of response.headers) res.setHeader(key, value);
      if (!response.body || method === 'HEAD') {
        await response.body?.cancel();
        res.end();
        return;
      }
      source = Readable.fromWeb(response.body);
      await new Promise((resolve, reject) => {
        const fail = (err) => reject(err);
        const done = () => resolve();
        source.once('error', fail);
        res.once('error', fail);
        res.once('finish', done);
        res.once('close', done);
        source.pipe(res);
      });
    } catch (err) {
      source?.destroy();
      if (!res.headersSent && next) next(err);
      else if (!res.headersSent) {
        res.statusCode = 500;
        res.end();
      } else res.destroy(err);
    } finally {
      res.removeListener('close', abort);
      if (!res.writableFinished) source?.destroy();
    }
  };
}
