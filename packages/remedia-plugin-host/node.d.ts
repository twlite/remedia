import type { IncomingMessage, ServerResponse } from 'node:http';
import type { PluginHost } from './index.js';

/** Node HTTP, Express and Connect request/response adapter. */
export declare function createNodeMiddleware(host: Pick<PluginHost, 'fetch'>): (
  req: IncomingMessage,
  res: ServerResponse,
  next?: (error?: unknown) => void,
) => Promise<void>;
