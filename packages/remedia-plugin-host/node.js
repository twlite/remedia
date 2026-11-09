import { Readable } from 'node:stream';

/**
 * Connect/Express and Node.js HTTP middleware. Mount before any body parser.
 * @param {{fetch(request: Request): Promise<Response>}} host Fetch-compatible Remedia plugin host.
 * @returns {(req: import('node:http').IncomingMessage, res: import('node:http').ServerResponse, next?: (error?: unknown) => void) => Promise<void>} Node middleware.
 */
export function createNodeMiddleware(host) {
  return async (req, res, next) => {
    const controller = new AbortController();
    const abort = () => controller.abort();
    res.once('close', abort);
    try {
      const url = new URL(req.url || '/', 'http://' + (req.headers.host || 'localhost'));
      const method = req.method || 'GET';
      const request = new Request(url, {
        method,
        headers: req.headers,
        body: method === 'GET' || method === 'HEAD' ? undefined : Readable.toWeb(req),
        duplex: 'half',
        signal: controller.signal,
      });
      const response = await host.fetch(request);
      res.statusCode = response.status;
      for (const [key, value] of response.headers) res.setHeader(key, value);
      if (response.body) {
        await new Promise((resolve, reject) => {
          const source = Readable.fromWeb(response.body);
          source.once('error', reject);
          res.once('error', reject);
          res.once('finish', resolve);
          res.once('close', resolve);
          source.pipe(res);
        });
      } else {
        res.end();
      }
    } catch (err) {
      if (!res.headersSent && next) next(err);
      else if (!res.headersSent) {
        res.statusCode = 500;
        res.end();
      } else res.destroy(err);
    } finally {
      res.removeListener('close', abort);
    }
  };
}
