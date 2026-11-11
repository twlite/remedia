import { z } from 'zod';

/** Stable schema artifact served by the Remedia documentation site. */
export const REMEDIA_CONFIG_SCHEMA_URL =
  'https://twlite.github.io/remedia/schemas/remedia.config.schema.json';

const environment = z
  .string()
  .regex(/^env\('[A-Za-z_][A-Za-z0-9_]*'\)$/)
  .describe(
    "Runtime environment reference, for example env('REMEDIA_RPC_PASSWORD'). Validation never resolves it.",
  );
const literal = z
  .string()
  .refine((value) => !value.startsWith('env('), 'Malformed environment reference');
const bytes = (value: string): number => new TextEncoder().encode(value).length;
const username = z
  .union([
    environment,
    literal.refine(
      (value) => bytes(value) >= 1 && bytes(value) <= 128 && !value.includes(':'),
      'Username must be 1–128 UTF-8 bytes and cannot contain a colon',
    ),
  ])
  .describe(
    'Basic authentication username; literal or runtime environment reference. Resolved usernames must be unique.',
  );
const password = z
  .union([
    environment,
    literal.refine(
      (value) => bytes(value) >= 1 && bytes(value) <= 4096,
      'Password must be 1–4096 UTF-8 bytes',
    ),
  ])
  .describe(
    'Basic authentication password; literal or runtime environment reference. Never exposed by configuration inspection.',
  );
const integer = (min: number, max: number, value: number, description: string) =>
  z.number().int().min(min).max(max).default(value).describe(description);
const hostPattern = (input: string): boolean => {
  const value = input.trim().replace(/\.+$/, '').toLowerCase();
  return (
    value === '*' ||
    (value.length > 0 &&
      value.length <= 253 &&
      value
        .split('.')
        .every((label) => label.length <= 63 && /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(label)))
  );
};
const host = z
  .union([environment, literal.refine(hostPattern, 'Expected a DNS host name or *')])
  .describe(
    'Allowed DNS host and its subdomains, or * for all hosts. Literal or runtime environment reference.',
  );
const limits = z
  .strictObject({
    maxConnections: integer(1, 65536, 128, 'Maximum concurrent RPC connections.'),
    maxClientSessions: integer(
      1,
      10000,
      128,
      'Maximum resumable client sessions per authenticated username.',
    ),
    maxTotalPlayers: integer(1, 10000, 256, 'Maximum allocated players across the server.'),
    maxPlayersPerClient: integer(
      1,
      1000,
      16,
      'Maximum players per client; cannot exceed maxTotalPlayers.',
    ),
    maxMessageBytes: integer(1024, 1048576, 65536, 'Maximum RPC message size in bytes.'),
    requestTimeoutMs: integer(1, 300000, 30000, 'Request and handshake deadline in milliseconds.'),
    maxInFlightRequests: integer(1, 4096, 16, 'Maximum simultaneous requests per WebSocket.'),
    maxRequestsPerSecond: integer(1, 100000, 60, 'Maximum requests per second per WebSocket.'),
    eventBuffer: integer(1, 65536, 128, 'Retained events per client session.'),
  })
  .prefault({})
  .refine(
    (value) => value.maxPlayersPerClient <= value.maxTotalPlayers,
    'maxPlayersPerClient cannot exceed maxTotalPlayers',
  );

const identifier = z
  .string()
  .regex(/^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$/, 'Expected 1–128 letters, digits, dots, _ or -');
const isOrigin = (value: string): boolean => {
  try {
    const url = new URL(value.trim());
    return (
      (url.protocol === 'http:' || url.protocol === 'https:') &&
      url.username === '' &&
      url.password === '' &&
      url.search === '' &&
      url.hash === '' &&
      url.pathname === '/'
    );
  } catch {
    return false;
  }
};
const origin = z
  .union([
    environment,
    literal.refine(
      isOrigin,
      'Expected an HTTP(S) origin without credentials, path, query or fragment',
    ),
  ])
  .describe('HTTP(S) origin; literal or runtime environment reference.');
/** Matches the Rust loader: ASCII space or ASCII control characters. */
const isSpaceOrControl = (character: string): boolean => {
  const code = character.charCodeAt(0);
  return code <= 0x20 || code === 0x7f;
};
const pluginToken = z
  .union([
    environment,
    literal.refine(
      (value) => bytes(value) >= 16 && bytes(value) <= 4096 && ![...value].some(isSpaceOrControl),
      'Token must be 16–4096 UTF-8 bytes without spaces or control characters',
    ),
  ])
  .describe('Bearer token for the configured plugin-host origin; never sent to media URLs.');
const pluginHostLimits = z
  .strictObject({
    requestTimeoutMs: integer(
      1,
      300000,
      10000,
      'Connect and total deadline for each discovery or execute request, in milliseconds.',
    ),
    discoveryTtlMs: integer(1000, 3600000, 60000, 'Discovery refresh interval in milliseconds.'),
    maxRequestBytes: integer(64, 16777216, 65536, 'Maximum serialized execute request in bytes.'),
    maxResponseBytes: integer(
      256,
      16777216,
      65536,
      'Maximum discovery or execute response body in bytes.',
    ),
    maxPendingOperations: integer(
      1,
      1024,
      8,
      'Maximum concurrent operation preparations for this host.',
    ),
    maxActiveStreams: integer(1, 1024, 8, 'Maximum concurrent media transfers for this host.'),
    maxMediaBytes: integer(
      1024,
      68719476736,
      1073741824,
      'Maximum bytes accepted for one media transfer.',
    ),
    mediaIdleTimeoutMs: integer(
      1,
      300000,
      15000,
      'Maximum wait for the next media chunk, in milliseconds.',
    ),
    mediaBufferBytes: integer(
      4096,
      16777216,
      262144,
      'Per-stream queue between the network and the decoder, in bytes.',
    ),
    maxAttempts: integer(1, 3, 2, 'Total attempts for replay-safe operations.'),
  })
  .prefault({})
  .describe('Per-host resource limits.');
const pluginHost = z
  .strictObject({
    id: identifier.describe('Stable, unique deployment ID used by clients.'),
    enabled: z
      .boolean()
      .default(true)
      .describe(
        'Disabled hosts keep their configuration but are not registered as executable at startup.',
      ),
    endpoint: origin.describe('Discovery and execution origin; https unless allowInsecureHttp.'),
    auth: z
      .discriminatedUnion('type', [
        z.strictObject({ type: z.literal('none') }),
        z.strictObject({ type: z.literal('bearer'), token: pluginToken }),
      ])
      .default({ type: 'none' })
      .describe('Authentication sent only to the configured endpoint origin.'),
    allowInsecureHttp: z
      .boolean()
      .default(false)
      .describe('Permit http: endpoints and media origins.'),
    allowPrivateNetwork: z
      .boolean()
      .default(false)
      .describe('Permit loopback, private and link-local addresses for this host.'),
    mediaOrigins: z
      .array(origin)
      .default([])
      .describe(
        'Extra origins fetched without credentials; the endpoint origin is always included.',
      ),
    allowExternalMedia: z
      .boolean()
      .default(false)
      .describe('Allow descriptor URLs on other origins under the ordinary media.* policy.'),
    operations: z
      .array(
        z.strictObject({
          pluginId: identifier,
          operation: identifier,
          replaySafe: z
            .boolean()
            .default(false)
            .describe('Executing again has no side effects; enables retries and grant refresh.'),
        }),
      )
      .min(1)
      .max(256)
      .refine(
        (operations) =>
          new Set(operations.map((item) => `${item.pluginId}\u0000${item.operation}`)).size ===
          operations.length,
        'Operations must be unique',
      )
      .describe('Allowed (pluginId, operation) pairs.'),
    limits: pluginHostLimits,
  })
  .refine(
    (entry) =>
      entry.allowInsecureHttp ||
      entry.endpoint.startsWith('env(') ||
      entry.endpoint.trim().toLowerCase().startsWith('https:'),
    'http endpoints require allowInsecureHttp',
  )
  .describe('A generic plugin-host deployment.');

/** Strict server configuration schema. Applies defaults without resolving secrets
 * or touching the filesystem. Rust startup validation remains authoritative for
 * resolved values, media directory access, and process environment/file behavior.
 */
export const RemediaConfigSchema = z
  .strictObject({
    $schema: z
      .string()
      .nullable()
      .optional()
      .describe('Editor JSON Schema URI; metadata only, never fetched by the server.'),
    bind: z
      .strictObject({
        host: z
          .union([
            environment,
            literal.refine((value) => value.trim().length > 0, 'Host cannot be blank'),
          ])
          .default('127.0.0.1')
          .describe('Bind host; literal or runtime environment reference.'),
        port: integer(0, 65535, 8787, 'TCP listen port; zero requests an ephemeral port.'),
      })
      .prefault({})
      .describe('RPC listen address.'),
    users: z
      .array(z.strictObject({ username, password }))
      .min(1)
      .refine(
        (users) => new Set(users.map((user) => user.username)).size === users.length,
        'Usernames must be unique',
      )
      .describe(
        'Required authenticated users; duplicate resolved usernames are rejected at startup.',
      ),
    sessionResumptionTimeoutSeconds: integer(
      1,
      86400,
      60,
      'Disconnected client session retention in seconds.',
    ),
    limits,
    media: z
      .strictObject({
        allowedFileRoots: z
          .array(z.union([environment, literal]))
          .default([])
          .describe(
            'Server-local directories; relative to the config file. ~ expands at startup. Roots must exist and be directories; empty denies file playback.',
          ),
        allowedUrlHosts: z
          .array(host)
          .default([])
          .refine((hosts) => {
            const literals = hosts
              .filter((value) => !value.startsWith('env('))
              .map((value) => value.trim().replace(/\.+$/, '').toLowerCase());
            return !literals.includes('*') || literals.every((value) => value === '*');
          }, 'Cannot combine * with specific hosts')
          .describe(
            'Allowed DNS hosts and subdomains. Empty denies URL playback; wildcard cannot be mixed with specific resolved hosts.',
          ),
        allowHttp: z
          .boolean()
          .default(true)
          .describe('Permit HTTP media URLs, subject to host and network policy.'),
        allowHttps: z
          .boolean()
          .default(true)
          .describe('Permit HTTPS media URLs, subject to host and network policy.'),
        blockPrivateNetworks: z
          .boolean()
          .default(true)
          .describe('Reject media URLs resolving to private or reserved network addresses.'),
      })
      .prefault({})
      .describe('Media access policy, evaluated on the server host.'),
    pluginHosts: z
      .array(pluginHost)
      .max(64)
      .default([])
      .refine(
        (hosts) => new Set(hosts.map((item) => item.id)).size === hosts.length,
        'Plugin host IDs must be unique',
      )
      .describe('Generic plugin-host deployments whose allowed operations can produce audio.'),
    audio: z
      .strictObject({
        opus: z
          .strictObject({
            bitrate: integer(
              500,
              512000,
              128000,
              'Opus target bitrate in bits per second. Does not affect passthrough.',
            ),
            complexity: integer(
              0,
              10,
              9,
              'Opus encoder complexity; lower values trade quality for CPU. Default remains 9.',
            ),
            vbr: z.boolean().default(true).describe('Allow variable bitrate for encoded Opus.'),
          })
          .prefault({})
          .describe('PCM-to-Opus encoder settings.'),
        crossfade: z
          .strictObject({
            enabled: z
              .boolean()
              .default(false)
              .describe('Allow clients to opt into crossfade; each session must also opt in.'),
            durationMs: integer(1000, 30000, 5000, 'Overlap duration in milliseconds.'),
            prepareBeforeMs: integer(
              5000,
              120000,
              30000,
              'Preparation lead before overlap begins, in milliseconds.',
            ),
            curve: z
              .literal('equalPower')
              .default('equalPower')
              .describe('Sample-accurate equal-power overlap curve.'),
          })
          .prefault({})
          .describe('Server crossfade permission and defaults.'),
      })
      .prefault({})
      .describe('Native audio processing defaults.'),
    observability: z
      .strictObject({
        console: z
          .boolean()
          .default(true)
          .describe(
            'Enable console diagnostics; exporter configuration uses existing runtime environment settings.',
          ),
      })
      .prefault({})
      .describe('Local observability settings.'),
  })
  .describe(
    'Remedia server configuration. Unknown fields are rejected at every level. Environment references are resolved only by the Rust runtime.',
  );

/** User-authored configuration input, with optional defaulted fields. */
export type RemediaConfig = z.input<typeof RemediaConfigSchema>;

/** Generate JSON Schema draft 2020-12 from the runtime schema. Custom refinements
 * (byte lengths, cross-field constraints, environment/filesystem resolution) still
 * require runtime validation and are described in field metadata.
 * @returns A deterministic JSON Schema object suitable for JSON-aware editors.
 */
export function generateJsonSchema(): Record<string, unknown> {
  return {
    ...z.toJSONSchema(RemediaConfigSchema, { target: 'draft-2020-12', io: 'input' }),
    $id: REMEDIA_CONFIG_SCHEMA_URL,
  };
}
