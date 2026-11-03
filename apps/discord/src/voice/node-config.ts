import { z } from 'zod';
import type { RemoteNodeConfig } from '@remedia/client';

const remoteNodeSchema = z
  .object({
    id: z.string().min(1),
    url: z.string().min(1),
    auth: z
      .object({
        username: z.string().min(1),
        password: z.string(),
      })
      .strict(),
    capacity: z.number().int().positive().optional(),
  })
  .strict();

/** Parse REMEDIA_NODES as a JSON array without printing credentials in validation errors. */
export function remoteNodesFromEnvironment(raw: string | undefined): RemoteNodeConfig[] {
  if (!raw) {
    throw new Error('REMEDIA_NODES must contain a JSON array when REMEDIA_MODE=remote.');
  }
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new Error('REMEDIA_NODES must be valid JSON.');
  }
  const parsed = z.array(remoteNodeSchema).min(1).safeParse(value);
  if (!parsed.success) {
    throw new Error(
      'REMEDIA_NODES must contain nodes with id, URL, auth.username, and auth.password.',
    );
  }
  return parsed.data.map(({ capacity, ...node }) =>
    capacity === undefined ? { type: 'remote', ...node } : { type: 'remote', ...node, capacity },
  );
}
