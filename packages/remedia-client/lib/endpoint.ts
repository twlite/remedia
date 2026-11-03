import type { RemoteNodeAuth } from './types.js';

export interface ParsedEndpoint {
  readonly url: string;
  readonly authorization: string;
}

/** Parse Remedia and standard WebSocket URLs without retaining credentials in the result URL. */
export function parseRemoteEndpoint(input: string, explicitAuth?: RemoteNodeAuth): ParsedEndpoint {
  let url: URL;
  const normalizedInput = input.replace(/^(remedia|remedias):/i, (_scheme, value: string) =>
    value.toLowerCase() === 'remedia' ? 'ws:' : 'wss:',
  );
  try {
    url = new URL(normalizedInput);
  } catch {
    throw new TypeError('Remote node URL must be a valid remedia, remedias, ws, or wss URL');
  }

  if (url.protocol !== 'ws:' && url.protocol !== 'wss:') {
    throw new TypeError('Remote node URL scheme must be remedia, remedias, ws, or wss');
  }
  if (!url.hostname) throw new TypeError('Remote node URL must include a hostname');
  if (url.pathname === '/' || url.pathname === '') url.pathname = '/v1/ws';

  let embeddedAuth: RemoteNodeAuth | undefined;
  if (url.username || url.password) {
    try {
      embeddedAuth = {
        username: decodeURIComponent(url.username),
        password: decodeURIComponent(url.password),
      };
    } catch {
      throw new TypeError('Remote node URL has malformed encoded credentials');
    }
  }
  url.username = '';
  url.password = '';

  const auth = explicitAuth ?? embeddedAuth;
  if (!auth) throw new TypeError('Remote node authentication is required');
  if (
    auth.username.length === 0 ||
    auth.password.length === 0 ||
    auth.username.includes(':') ||
    /[\r\n]/.test(auth.username) ||
    /[\r\n]/.test(auth.password)
  ) {
    throw new TypeError('Remote node authentication fields are malformed');
  }

  const authorization = `Basic ${Buffer.from(`${auth.username}:${auth.password}`, 'utf8').toString('base64')}`;
  return { url: url.toString(), authorization };
}
