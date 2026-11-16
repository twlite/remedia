import { createReadStream } from 'node:fs';
import { createHash } from 'node:crypto';
import { setTimeout } from 'node:timers/promises';

export async function fileHash(path, algorithm = 'sha256', encoding = 'hex') {
  const hash = createHash(algorithm);
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest(encoding);
}

export function describeError(error) {
  const nested = error?.errors?.map(describeError) ?? [];
  return [
    error?.message ?? String(error),
    error?.code,
    ...nested,
    ...(error?.cause ? [describeError(error.cause)] : []),
  ]
    .filter(Boolean)
    .join(': ');
}

export async function requestWithRetry(
  url,
  options = {},
  { fetchImpl = fetch, wait = setTimeout, attempts = 4 } = {},
) {
  const operation = `${options.method ?? 'GET'} ${url}`;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      const response = await fetchImpl(url, { signal: AbortSignal.timeout(60_000), ...options });
      if (response.ok || ![408, 429, 500, 502, 503, 504].includes(response.status)) return response;
      if (attempt === attempts)
        throw new Error(`${operation}: HTTP ${response.status} after ${attempts} attempts`);
      await response.body?.cancel();
      const retryAfter = Number(response.headers?.get('retry-after'));
      await wait(Math.min(30_000, Math.max(1000 * 2 ** (attempt - 1), (retryAfter || 0) * 1000)));
    } catch (error) {
      if (attempt === attempts)
        throw new Error(`${operation} failed: ${describeError(error)}`, { cause: error });
      await wait(1000 * 2 ** (attempt - 1));
    }
  }
}
