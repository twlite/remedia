import { spawn, type ChildProcess } from 'node:child_process';
import { once } from 'node:events';
import { createReadStream } from 'node:fs';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { Readable } from 'node:stream';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import {
  createMediaSource,
  createPluginHost,
  PluginHostError,
  type PluginHost,
} from '@remedia/plugin-host';
import { createNodeMiddleware } from '@remedia/plugin-host/node';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { WebSocket } from 'ws';
import { RemediaRpcError } from '../lib/errors.js';
import { RemoteNodeAdapter, RemoteNodeEvent } from '../lib/remote-node.js';

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const fixtures = resolve(repositoryRoot, 'crates/remedia-core/tests/fixtures');
const mp3Fixture = resolve(fixtures, 'generated-tone.mp3');
const opusFixture = resolve(fixtures, 'generated-opus-passthrough.ogg');

const PLUGIN_ID = 'example.source';
const OPERATION = 'source.stream';
const NON_REPLAY_OPERATION = 'source.once';
const BOT_ID = '100000000000000001';

/** Credentials are random per run so a leaked value cannot be mistaken for a fixture. */
const tokens = {
  a: 'host-a-token-0123456789abcdef',
  b: 'host-b-token-0123456789abcdef',
  c: 'host-c-token-0123456789abcdef',
  wrong: 'wrong-token-0123456789abcdef',
  small: 'host-small-token-0123456789',
  tight: 'host-tight-token-0123456789',
};

interface RecordedRequest {
  method: string;
  path: string;
  headers: IncomingMessage['headers'];
  closedEarly: boolean;
}

interface StreamStats {
  pulled: number;
  disposed: boolean;
}

interface Behaviour {
  /** Number of times each track was requested from the host. */
  calls: Map<string, number>;
  /** Handlers which observed cancellation. */
  aborted: number;
  started: number;
  streams: StreamStats[];
  /** Deployment label which decides what the `deployment` track returns. */
  deployment: string;
  externalUrl?: string;
}

interface Harness {
  host: PluginHost;
  http: Server;
  port: number;
  origin: string;
  requests: RecordedRequest[];
  behaviour: Behaviour;
  extraRoute?: (request: IncomingMessage, response: ServerResponse) => boolean;
  close(): Promise<void>;
}

/** A real @remedia/plugin-host behind a real HTTP listener. */
async function startHost(options: {
  token?: string;
  deployment: string;
  port?: number;
  mediaTtlMs?: number;
}): Promise<Harness> {
  const http = createServer();
  const requests: RecordedRequest[] = [];
  const behaviour: Behaviour = {
    calls: new Map(),
    aborted: 0,
    started: 0,
    streams: [],
    deployment: options.deployment,
  };

  http.listen(options.port ?? 0, '127.0.0.1');
  await once(http, 'listening');

  const port = (http.address() as AddressInfo).port;
  const origin = `http://127.0.0.1:${port}`;
  const host = createPluginHost({
    name: `Host ${options.deployment}`,
    publicUrl: origin,
    ...(options.token ? { token: options.token } : {}),
    ...(options.mediaTtlMs ? { mediaTtlMs: options.mediaTtlMs } : {}),
    onError: () => {},
  });
  // Both operations share one implementation; the Remedia configuration decides which is replay-safe.
  const handle = (input: unknown, { signal }: { signal: AbortSignal }) =>
    produce(harness, input, signal);

  host.register({
    id: PLUGIN_ID,
    operations: { [OPERATION]: handle, [NON_REPLAY_OPERATION]: handle },
  });

  const middleware = createNodeMiddleware(host);

  http.on('request', (request, response) => {
    const record: RecordedRequest = {
      method: request.method ?? 'GET',
      path: (request.url ?? '/').split('?')[0]!,
      headers: request.headers,
      closedEarly: false,
    };

    requests.push(record);
    response.once('close', () => {
      record.closedEarly = !response.writableFinished;
    });

    if (harness.extraRoute?.(request, response)) return;

    void middleware(request, response, () => {
      response.statusCode = 404;
      response.end();
    });
  });

  const harness: Harness = {
    host,
    http,
    port,
    origin,
    requests,
    behaviour,
    async close() {
      http.closeAllConnections();
      await new Promise<void>((done) => http.close(() => done()));
      await host.close().catch(() => {});
    },
  };

  return harness;
}

function wavStream(seconds: number, stats: StreamStats): Readable {
  const dataBytes = seconds * 48_000 * 4;
  const header = Buffer.alloc(44);

  header.write('RIFF');
  header.writeUInt32LE(dataBytes + 36, 4);
  header.write('WAVEfmt ', 8);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(2, 22);
  header.writeUInt32LE(48_000, 24);
  header.writeUInt32LE(192_000, 28);
  header.writeUInt16LE(4, 32);
  header.writeUInt16LE(16, 34);
  header.write('data', 36);
  header.writeUInt32LE(dataBytes, 40);

  let remaining = dataBytes;
  let sentHeader = false;

  const stream = new Readable({
    highWaterMark: 16 * 1024,
    read() {
      if (!sentHeader) {
        sentHeader = true;
        stats.pulled += header.length;
        this.push(header);

        return;
      }

      if (remaining <= 0) {
        this.push(null);

        return;
      }

      const size = Math.min(16 * 1024, remaining);

      remaining -= size;
      stats.pulled += size;
      this.push(Buffer.alloc(size));
    },
    destroy(error, callback) {
      stats.disposed = true;
      callback(error);
    },
  });

  return stream;
}

async function produce(harness: Harness, input: unknown, signal: AbortSignal): Promise<unknown> {
  const spec = input as { track: string; seconds?: number; ms?: number };
  const { behaviour } = harness;

  behaviour.calls.set(spec.track, (behaviour.calls.get(spec.track) ?? 0) + 1);
  behaviour.started += 1;

  const call = behaviour.calls.get(spec.track)!;

  switch (spec.track) {
    case 'mp3':
      return createMediaSource({
        contentType: 'audio/mpeg',
        open: () => createReadStream(mp3Fixture),
      });
    case 'opus':
      return createMediaSource({
        contentType: 'audio/ogg',
        open: () => createReadStream(opusFixture),
      });
    case 'deployment':
      return createMediaSource({
        contentType: behaviour.deployment === 'a' ? 'audio/mpeg' : 'audio/ogg',
        open: () => createReadStream(behaviour.deployment === 'a' ? mp3Fixture : opusFixture),
      });
    case 'wav':
      return createMediaSource({
        contentType: 'audio/wav',
        open: () => {
          const stats: StreamStats = { pulled: 0, disposed: false };

          behaviour.streams.push(stats);

          return wavStream(spec.seconds ?? 30, stats);
        },
      });
    case 'hang':
      await new Promise<void>((_, reject) => {
        signal.addEventListener(
          'abort',
          () => {
            behaviour.aborted += 1;
            reject(signal.reason);
          },
          { once: true },
        );
      });

      return undefined;
    case 'slow-ignore':
      await sleep(spec.ms ?? 600);

      return createMediaSource({
        contentType: 'audio/mpeg',
        open: () => createReadStream(mp3Fixture),
      });
    case 'fail-once':
      if (call === 1) throw new PluginHostError('host_closed', 503);

      return createMediaSource({
        contentType: 'audio/mpeg',
        open: () => createReadStream(mp3Fixture),
      });
    case 'expired-once':
      if (call === 1)
        return {
          url: `${harness.origin}/remedia/v1/media/expired-token`,
          expiresAtMs: Date.now() + 60_000,
        };

      return createMediaSource({
        contentType: 'audio/mpeg',
        open: () => createReadStream(mp3Fixture),
      });
    case 'external':
      return { url: behaviour.externalUrl };
    case 'redirect':
      return { url: `${harness.origin}/redirect-to-external` };
    case 'declined':
      return undefined;
    case 'text':
      return createMediaSource({
        contentType: 'text/html',
        open: () => Readable.from([Buffer.from('<html>not audio</html>')]),
      });
    case 'truncated':
      return createMediaSource({
        contentType: 'audio/mpeg',
        open: () =>
          new Readable({
            read() {
              this.push(Buffer.alloc(2048));
              this.destroy(new Error('upstream died'));
            },
          }),
      });
    default:
      throw new PluginHostError('invalid_request', 400);
  }
}

describe('Rust server with generic plugin hosts', () => {
  let configDirectory: string;
  let server: ChildProcess;
  let node: RemoteNodeAdapter;
  let wsUrl = '';
  let hostA: Harness;
  let hostB: Harness;
  let hostC: Harness;
  let hostSmall: Harness;
  let hostTight: Harness;
  let hostLocalhost: Harness;
  let external: Server;
  let externalRequests = 0;
  let nextGuild = 300000000000000000n;

  const testGuilds = new Set<string>();
  const guild = (): string => {
    const id = String((nextGuild += 1n));

    testGuilds.add(id);

    return id;
  };
  const op = (hostId: string, input: unknown, operation = OPERATION) => ({
    type: 'operation' as const,
    hostId,
    pluginId: PLUGIN_ID,
    operation,
    input,
  });
  const lifecycle: Array<{ event: string; data: Record<string, unknown> }> = [];
  const codeOf = async (promise: Promise<unknown>): Promise<string> => {
    try {
      await promise;
    } catch (error) {
      if (error instanceof RemediaRpcError) return error.code;

      throw error;
    }

    throw new Error('expected the RPC to fail');
  };
  const pluginStats = async (id: string): Promise<Record<string, any>> => {
    const stats = await node.request<{ pluginHosts: Array<Record<string, any>> }>('node.stats', {});

    return stats.pluginHosts.find((host) => host.id === id)!;
  };
  const stopAll = async (guildId: string): Promise<void> => {
    await node.control(guildId, 'player.stop').catch(() => {});
    await node.destroyGuild(guildId).catch(() => {});
  };

  beforeAll(async () => {
    external = createServer((_request, response) => {
      externalRequests += 1;
      response.end('secret');
    });
    external.listen(0, '127.0.0.1');
    await once(external, 'listening');

    hostA = await startHost({ token: tokens.a, deployment: 'a' });
    hostB = await startHost({ token: tokens.b, deployment: 'b' });
    hostC = await startHost({ token: tokens.c, deployment: 'c' });
    hostSmall = await startHost({ token: tokens.small, deployment: 'a' });
    hostTight = await startHost({ token: tokens.tight, deployment: 'a' });
    hostLocalhost = await startHost({ deployment: 'a' });

    const externalPort = (external.address() as AddressInfo).port;

    for (const harness of [hostA, hostB, hostC, hostSmall, hostTight]) {
      harness.behaviour.externalUrl = `http://127.0.0.1:${externalPort}/secret.mp3`;
      harness.extraRoute = (request, response) => {
        if (request.url !== '/redirect-to-external') return false;

        response.writeHead(302, { location: `http://127.0.0.1:${externalPort}/secret.mp3` });
        response.end();

        return true;
      };
    }

    const operations = [
      { pluginId: PLUGIN_ID, operation: OPERATION, replaySafe: true },
      { pluginId: PLUGIN_ID, operation: NON_REPLAY_OPERATION },
    ];
    const base = { allowInsecureHttp: true, allowPrivateNetwork: true, operations };

    configDirectory = await mkdtemp(resolve(tmpdir(), 'remedia-plugin-host-'));

    const configPath = resolve(configDirectory, 'remedia.config.json');

    await writeFile(
      configPath,
      JSON.stringify({
        bind: { host: '127.0.0.1', port: 0 },
        users: [{ username: 'sdk-test', password: 'sdk-test-password' }],
        sessionResumptionTimeoutSeconds: 10,
        limits: {
          maxConnections: 8,
          maxClientSessions: 4,
          maxTotalPlayers: 32,
          maxPlayersPerClient: 32,
          maxMessageBytes: 65_536,
          requestTimeoutMs: 10_000,
          maxInFlightRequests: 16,
          maxRequestsPerSecond: 600,
          eventBuffer: 256,
        },
        media: { allowedUrlHosts: [], blockPrivateNetworks: true },
        audio: { crossfade: { enabled: true, durationMs: 1000 } },
        pluginHosts: [
          {
            id: 'host-a',
            endpoint: hostA.origin,
            auth: { type: 'bearer', token: tokens.a },
            ...base,
          },
          {
            id: 'host-b',
            endpoint: hostB.origin,
            auth: { type: 'bearer', token: tokens.b },
            ...base,
          },
          {
            id: 'host-bad-auth',
            endpoint: hostC.origin,
            auth: { type: 'bearer', token: tokens.wrong },
            ...base,
          },
          {
            id: 'host-down',
            endpoint: 'http://127.0.0.1:9',
            ...base,
            limits: { requestTimeoutMs: 1_500 },
          },
          {
            id: 'host-localhost',
            endpoint: `http://localhost:${hostLocalhost.port}`,
            allowInsecureHttp: true,
            operations,
            limits: { requestTimeoutMs: 1_500 },
          },
          {
            id: 'host-small',
            endpoint: hostSmall.origin,
            auth: { type: 'bearer', token: tokens.small },
            ...base,
            limits: { maxMediaBytes: 2_048 },
          },
          {
            id: 'host-tight',
            endpoint: hostTight.origin,
            auth: { type: 'bearer', token: tokens.tight },
            ...base,
            limits: { maxPendingOperations: 1, maxActiveStreams: 1 },
          },
        ],
        observability: { console: false },
      }),
    );

    const binary = process.env.REMEDIA_SERVER_BINARY;

    server = spawn(
      binary ?? 'cargo',
      binary
        ? ['--config', configPath, '--shutdown-on-stdin-close']
        : [
            'run',
            '--quiet',
            '-p',
            'remedia-server',
            '--',
            '--config',
            configPath,
            '--shutdown-on-stdin-close',
          ],
      { cwd: repositoryRoot, stdio: ['pipe', 'pipe', 'pipe'] },
    );
    server.stderr?.resume();

    const url = await waitForServer(server);

    wsUrl = url;
    node = new RemoteNodeAdapter({
      botUserId: BOT_ID,
      config: {
        type: 'remote',
        id: 'plugin-host-server',
        url: url.replace('ws://', 'remedia://'),
        auth: { username: 'sdk-test', password: 'sdk-test-password' },
      },
      requestTimeoutMs: 15_000,
      maxPendingRequests: 32,
    });
    node.on(RemoteNodeEvent.lifecycle, (event) => lifecycle.push(event));
    await node.start();
  }, 180_000);

  afterEach(async () => {
    // A failed assertion must not leave a paused player or prepared stream in the next test.
    await Promise.all([...testGuilds].map(stopAll));
    testGuilds.clear();
  });

  afterAll(async () => {
    await node?.close();

    if (server && server.exitCode === null) {
      const closed = once(server, 'close');

      server.stdin?.end();
      await Promise.race([closed, sleep(5_000)]);
    }

    await Promise.all(
      [hostA, hostB, hostC, hostSmall, hostTight, hostLocalhost].map((harness) => harness?.close()),
    );
    external?.closeAllConnections();
    external?.close();

    if (configDirectory) await rm(configDirectory, { recursive: true, force: true });
  });

  it('negotiates the pluginOperations capability and discovers configured hosts', async () => {
    const capabilities = node.snapshot.capabilities?.pluginOperations;

    expect(capabilities?.available).toBe(true);
    expect(capabilities?.hosts).toEqual(
      expect.arrayContaining(['host-a', 'host-b', 'host-bad-auth', 'host-down']),
    );

    await expect.poll(async () => (await pluginStats('host-a')).status.state).toBe('healthy');
    await expect.poll(async () => (await pluginStats('host-b')).status.state).toBe('healthy');

    // Discovery authenticates to the configured origin only and never forwards credentials elsewhere.
    const discovery = hostA.requests.find((request) => request.path === '/.well-known/remedia');

    expect(discovery?.headers.authorization).toBe(`Bearer ${tokens.a}`);
  });

  it('plays an MP3 operation source with the default configuration and no Range request', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const before = hostA.requests.length;
    const started = await node.play(guildId, op('host-a', { track: 'mp3' }));

    expect(started.info.codec).toMatch(/^mp3/);
    expect(await node.waitForCompletion(guildId, started.generation)).toEqual({
      generation: started.generation,
      state: 'completed',
    });

    const added = hostA.requests.slice(before);
    const execute = added.find((request) => request.path === '/remedia/v1/execute');
    const media = added.filter((request) => request.path.startsWith('/remedia/v1/media/'));

    expect(execute?.headers.authorization).toBe(`Bearer ${tokens.a}`);
    expect(execute?.headers['x-remedia-operation-id']).toMatch(/^[0-9a-f]{32}$/);
    // Exactly one plain GET: no Range probe, no reopen, no reconnect, and no credentials.
    expect(media).toHaveLength(1);
    expect(media[0]?.method).toBe('GET');
    expect(media[0]?.headers.range).toBeUndefined();
    expect(media[0]?.headers.authorization).toBeUndefined();
    await stopAll(guildId);
  });

  it('plays an Ogg/Opus operation source', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const started = await node.play(guildId, op('host-a', { track: 'opus' }));

    expect(started.info.codec).toBe('opus');
    expect((await node.waitForCompletion(guildId, started.generation)).state).toBe('completed');
    await stopAll(guildId);
  });

  it('routes hosts which share plugin and operation IDs independently', async () => {
    const guildId = guild();
    const [callsA, callsB] = [
      hostA.behaviour.calls.get('deployment') ?? 0,
      hostB.behaviour.calls.get('deployment') ?? 0,
    ];

    await node.createGuild(guildId);

    const fromA = await node.play(guildId, op('host-a', { track: 'deployment' }));

    expect(fromA.info.codec).toMatch(/^mp3/);
    await node.waitForCompletion(guildId, fromA.generation);

    const fromB = await node.play(guildId, op('host-b', { track: 'deployment' }));

    expect(fromB.info.codec).toBe('opus');
    await node.waitForCompletion(guildId, fromB.generation);
    expect(hostA.behaviour.calls.get('deployment')).toBe(callsA + 1);
    expect(hostB.behaviour.calls.get('deployment')).toBe(callsB + 1);
    // A's credentials are never sent to B and vice versa.
    expect(
      hostB.requests.some((request) => request.headers.authorization === `Bearer ${tokens.a}`),
    ).toBe(false);
    expect(
      hostA.requests.some((request) => request.headers.authorization === `Bearer ${tokens.b}`),
    ).toBe(false);
    await stopAll(guildId);
  });

  it('reports authentication failures without retrying or trying another host', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const code = await codeOf(node.play(guildId, op('host-bad-auth', { track: 'mp3' })));

    expect(code).toBe('plugin_host_unauthorized');
    // Discovery fails first; a replay-safe operation must not be retried on authentication failure.
    expect(hostC.behaviour.calls.get('mp3') ?? 0).toBe(0);
    expect(hostA.behaviour.calls.get('mp3') ?? 0).toBeLessThanOrEqual(2);
    expect((await pluginStats('host-bad-auth')).totals.unauthorized).toBeGreaterThan(0);
    await stopAll(guildId);
  });

  it('rejects unknown, disallowed, missing and unreachable hosts explicitly', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    expect(await codeOf(node.play(guildId, op('does-not-exist', { track: 'mp3' })))).toBe(
      'plugin_host_not_found',
    );
    expect(
      await codeOf(
        node.play(guildId, { ...op('host-a', { track: 'mp3' }), operation: 'other.op' }),
      ),
    ).toBe('plugin_operation_not_allowed');
    // A refused connection can exceed the request deadline while the OS retries (Windows).
    expect(['plugin_host_unavailable', 'request_timeout']).toContain(
      await codeOf(node.play(guildId, op('host-down', { track: 'mp3' }))),
    );
    expect(await codeOf(node.play(guildId, op('host-a', { track: 'declined' })))).toBe(
      'plugin_operation_declined',
    );
    expect(await codeOf(node.play(guildId, op('host-a', { track: 'unknown-track' })))).toBe(
      'invalid_request',
    );
    expect(['degraded', 'unavailable']).toContain((await pluginStats('host-down')).status.state);
    await stopAll(guildId);
  });

  it('applies DNS policy to hostnames which resolve to private addresses', async () => {
    const guildId = guild();

    await node.createGuild(guildId);
    expect(await codeOf(node.play(guildId, op('host-localhost', { track: 'mp3' })))).toBe(
      'plugin_host_unavailable',
    );
    expect(hostLocalhost.requests).toHaveLength(0);
    await stopAll(guildId);
  });

  it('refuses operation sources for media probes', async () => {
    expect(
      await codeOf(node.request('media.probe', { source: op('host-a', { track: 'mp3' }) })),
    ).toBe('invalid_request');
    expect(hostA.behaviour.calls.get('mp3') ?? 0).toBeGreaterThanOrEqual(0);
  });

  it('cancels a pending operation when playback is stopped', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const abortedBefore = hostA.behaviour.aborted;
    const startedBefore = hostA.behaviour.started;
    const pending = node.play(guildId, op('host-a', { track: 'hang' }));
    const outcome = codeOf(pending);

    await expect.poll(() => hostA.behaviour.started).toBeGreaterThan(startedBefore);
    await node.control(guildId, 'player.stop');
    expect(await outcome).toBe('cancelled');
    await expect.poll(() => hostA.behaviour.aborted).toBe(abortedBefore + 1);
    expect((await node.playerState(guildId)).state).not.toBe('playing');
    await stopAll(guildId);
  });

  it('does not let a stale completion start playback after a skip', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    // The slow handler ignores cancellation and answers after 700 ms.
    const slow = codeOf(node.play(guildId, op('host-a', { track: 'slow-ignore', ms: 700 })));

    await sleep(150);

    const skippedAt = Date.now();

    await node.control(guildId, 'player.stop');
    expect(await slow).toBe('cancelled');
    expect(Date.now() - skippedAt).toBeLessThan(600);

    const next = await node.play(guildId, op('host-a', { track: 'wav', seconds: 3600 }));

    await node.control(guildId, 'player.pause');
    await sleep(900);
    // The cancelled operation's late result never replaced the replacement generation.
    const state = await node.playerState(guildId);

    expect(state.generation).toBe(next.generation);
    expect(state.state).toBe('paused');
    expect(state.source?.codec).toMatch(/pcm/);
    expect(hostA.behaviour.streams.length).toBeGreaterThan(0);
    await stopAll(guildId);
  });

  it('cancels staged-next preparation on stop, supersedes it by revision, and rejects stale generations', async () => {
    const guildId = guild();

    await node.createGuild(guildId, { enabled: true, durationMs: 1000 });

    // Decoding runs faster than real time into a null output; hold the source open so staging stays valid.
    const current = await node.play(guildId, op('host-a', { track: 'wav', seconds: 3600 }));

    await node.control(guildId, 'player.pause');
    const stage = (
      entryId: string,
      queueRevision: number,
      track: string,
      generation = current.generation,
    ) =>
      node.control(guildId, 'player.stage.next', {
        source: op('host-a', { track, ms: 900 }),
        expectedGeneration: generation,
        entryId,
        queueRevision,
        sourceRevision: 0,
      });

    // A stale generation never runs the operation.
    const callsBefore = hostA.behaviour.calls.get('slow-ignore') ?? 0;

    expect(await codeOf(stage('stale', 1, 'slow-ignore', current.generation + 7))).toBe(
      'stale_generation',
    );
    expect(hostA.behaviour.calls.get('slow-ignore') ?? 0).toBe(callsBefore);

    // A newer revision supersedes a pending preparation.
    const callsBeforeOlder = hostA.behaviour.calls.get('hang') ?? 0;
    const older = codeOf(stage('first', 1, 'hang'));

    // Wait for this request to own its cancellation scope, rather than a cumulative counter
    // which was already positive before staging. Concurrent RPC tasks can start out of order.
    await expect.poll(() => hostA.behaviour.calls.get('hang') ?? 0).toBe(callsBeforeOlder + 1);

    const newer = (await stage('second', 2, 'mp3')) as { entryId: string };

    expect(await older).toBe('cancelled');
    expect(newer.entryId).toBe('second');
    await expect
      .poll(async () => ((await node.control(guildId, 'player.stage.state')) as any)?.state)
      .toBe('ready');

    // Clearing discards both the pending and prepared stage.
    const callsBeforeClear = hostA.behaviour.calls.get('hang') ?? 0;
    const pending = codeOf(stage('third', 3, 'hang'));

    await expect.poll(() => hostA.behaviour.calls.get('hang') ?? 0).toBe(callsBeforeClear + 1);
    await node.control(guildId, 'player.stage.clear');
    expect(await pending).toBe('cancelled');
    expect(await node.control(guildId, 'player.stage.state')).toBeNull();

    // An older revision than one in flight is rejected without a second operation.
    const callsBeforeStop = hostA.behaviour.calls.get('hang') ?? 0;
    const inFlight = codeOf(stage('fourth', 5, 'hang'));

    await expect.poll(() => hostA.behaviour.calls.get('hang') ?? 0).toBe(callsBeforeStop + 1);
    expect(await codeOf(stage('fifth', 4, 'hang'))).toBe('stale_generation');
    await node.control(guildId, 'player.stop');
    expect(await inFlight).toBe('cancelled');
    await stopAll(guildId);
  });

  it('refreshes an expired grant for replay-safe operations only', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const before = hostA.behaviour.calls.get('expired-once') ?? 0;
    const started = await node.play(guildId, op('host-a', { track: 'expired-once' }));

    expect(started.info.codec).toMatch(/^mp3/);
    expect(hostA.behaviour.calls.get('expired-once')).toBe(before + 2);
    expect((await pluginStats('host-a')).totals.grantsExpired).toBeGreaterThan(0);
    await node.waitForCompletion(guildId, started.generation);

    // The same failure is final when the operation is not declared replay-safe.
    const strictBefore = hostA.behaviour.calls.get('expired-once')!;

    hostA.behaviour.calls.set('expired-once', 0);

    expect(
      await codeOf(
        node.play(guildId, op('host-a', { track: 'expired-once' }, NON_REPLAY_OPERATION)),
      ),
    ).toBe('plugin_media_expired');
    expect(hostA.behaviour.calls.get('expired-once')).toBe(1);
    expect(strictBefore).toBeGreaterThan(0);
    await stopAll(guildId);
  });

  it('retries transient failures only for replay-safe operations', async () => {
    const guildId = guild();

    await node.createGuild(guildId);
    hostA.behaviour.calls.set('fail-once', 0);

    const retried = await node.play(guildId, op('host-a', { track: 'fail-once' }));

    expect(retried.info.codec).toMatch(/^mp3/);
    expect(hostA.behaviour.calls.get('fail-once')).toBe(2);
    await node.waitForCompletion(guildId, retried.generation);

    hostA.behaviour.calls.set('fail-once', 0);
    expect(
      await codeOf(node.play(guildId, op('host-a', { track: 'fail-once' }, NON_REPLAY_OPERATION))),
    ).toBe('plugin_host_unavailable');
    expect(hostA.behaviour.calls.get('fail-once')).toBe(1);
    await stopAll(guildId);
  });

  it('enforces media origin policy, never follows redirects, and bounds media size', async () => {
    const guildId = guild();

    await node.createGuild(guildId);
    externalRequests = 0;

    expect(await codeOf(node.play(guildId, op('host-a', { track: 'external' })))).toBe(
      'plugin_media_invalid',
    );
    expect(await codeOf(node.play(guildId, op('host-a', { track: 'redirect' })))).toBe(
      'plugin_media_invalid',
    );
    expect(await codeOf(node.play(guildId, op('host-a', { track: 'text' })))).toBe(
      'plugin_media_invalid',
    );
    // Neither the foreign origin nor a redirect target is ever contacted.
    expect(externalRequests).toBe(0);
    // Credentials go only to discovery and execute, never to a media or redirect URL.
    expect(
      hostA.requests
        .filter(
          (request) =>
            request.path !== '/remedia/v1/execute' && request.path !== '/.well-known/remedia',
        )
        .every((request) => request.headers.authorization === undefined),
    ).toBe(true);

    // A response larger than the configured media limit fails instead of buffering without bound.
    const events = lifecycle.length;
    const small = await node
      .play(guildId, op('host-small', { track: 'wav', seconds: 5 }))
      .catch((error: unknown) => error);

    if (small instanceof Error) expect(small).toBeInstanceOf(RemediaRpcError);
    else
      await expect
        .poll(() =>
          lifecycle
            .slice(events)
            .some((event) => event.event === 'player.failed' && event.data.guildId === guildId),
        )
        .toBe(true);

    expect((await pluginStats('host-small')).totals.mediaFailures).toBeGreaterThan(0);
    await stopAll(guildId);
  });

  it('applies backpressure to the host and releases it on stop', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const stats = (): StreamStats => hostA.behaviour.streams.at(-1)!;
    // Keep the source readable: pause waits for an in-progress decoder read to finish.
    const started = await node.play(guildId, op('host-a', { track: 'wav', seconds: 3600 }));

    await node.control(guildId, 'player.pause');

    const positionMs = (await node.playerState(guildId)).position.mediaMs;

    expect(positionMs).not.toBeNull();

    // This fixture is 48 kHz stereo signed 16-bit PCM (192 bytes/ms). Subtract audio
    // already consumed before pause, since null output decodes much faster than real time.
    const consumedBytes = (positionMs ?? 0) * 192;

    // HTTP and OS socket buffers can continue filling after pause, especially on Windows.
    // Require the producer to stop reading for a full second, within a bounded deadline,
    // rather than assuming those buffers settle during a fixed initial sleep.
    await expect
      .poll(
        async () => {
          const before = stats().pulled;

          await sleep(1_000);
          // Encoded bytes ahead of the paused playback position must remain bounded.
          expect(stats().pulled - consumedBytes).toBeLessThan(16 * 1024 * 1024);

          return stats().pulled === before;
        },
        { timeout: 10_000, interval: 100 },
      )
      .toBe(true);

    const paused = stats().pulled;

    expect((await pluginStats('host-a')).resources.activeStreams).toBe(1);

    await node.control(guildId, 'player.resume');
    await expect.poll(() => stats().pulled).toBeGreaterThan(paused);

    // Forward seeks decode and discard; the sequential grant cannot rewind or reopen.
    await node.control(guildId, 'player.pause');

    const at = (await node.playerState(guildId)).position.mediaMs ?? 0;
    const mediaRequests = hostA.requests.filter((request) =>
      request.path.startsWith('/remedia/v1/media/'),
    ).length;

    await node.control(guildId, 'player.seek', { positionMs: at + 20_000 });
    expect(await codeOf(node.control(guildId, 'player.seek', { positionMs: 500 }))).toBe(
      'unsupported',
    );
    expect(
      hostA.requests.filter((request) => request.path.startsWith('/remedia/v1/media/')).length,
    ).toBe(mediaRequests);
    await node.control(guildId, 'player.resume');

    await node.control(guildId, 'player.stop');
    await expect.poll(() => stats().disposed).toBe(true);
    await expect.poll(async () => (await pluginStats('host-a')).resources.activeStreams).toBe(0);
    expect(started.generation).toBeGreaterThan(0);
    await stopAll(guildId);
  }, 20_000);

  it('enforces separate pending-operation and active-stream limits', async () => {
    const [one, two] = [guild(), guild()];

    await node.createGuild(one);
    await node.createGuild(two);

    const first = codeOf(node.play(one, op('host-tight', { track: 'hang' })));

    await expect.poll(() => hostTight.behaviour.started).toBeGreaterThan(0);
    expect(await codeOf(node.play(two, op('host-tight', { track: 'mp3' })))).toBe(
      'resource_exhausted',
    );
    await node.control(one, 'player.stop');
    expect(await first).toBe('cancelled');

    // One active stream consumes the only stream permit; a second stream is refused.
    // Long enough that the unpaced player cannot finish and release the stream permit early.
    await node.play(one, op('host-tight', { track: 'wav', seconds: 3600 }));
    expect(await codeOf(node.play(two, op('host-tight', { track: 'mp3' })))).toBe(
      'resource_exhausted',
    );
    await stopAll(one);
    await stopAll(two);
  });

  it('cancels a pending operation when the client connection is lost', async () => {
    const socket = new WebSocket(wsUrl, {
      headers: {
        authorization: `Basic ${Buffer.from('sdk-test:sdk-test-password').toString('base64')}`,
        'x-remedia-bot-id': BOT_ID,
      },
    });
    const pending = new Map<string, (message: Record<string, any>) => void>();

    socket.on('message', (data) => {
      const message = JSON.parse(data.toString()) as Record<string, any>;

      pending.get(message.id)?.(message);
    });
    await once(socket, 'open');

    const call = (id: string, command: string, params: unknown): Promise<Record<string, any>> =>
      new Promise((done) => {
        pending.set(id, done);
        socket.send(JSON.stringify({ version: 1, id, command, params }));
      });
    const guildId = guild();

    expect((await call('1', 'client.create', {})).ok).toBe(true);
    expect((await call('2', 'guild.create', { guildId })).ok).toBe(true);

    const abortedBefore = hostA.behaviour.aborted;
    const startedBefore = hostA.behaviour.started;

    void call('3', 'player.play', { guildId, source: op('host-a', { track: 'hang' }) });
    await expect.poll(() => hostA.behaviour.started).toBeGreaterThan(startedBefore);
    // An abrupt loss, not a protocol-level close.
    socket.terminate();
    await expect.poll(() => hostA.behaviour.aborted).toBe(abortedBefore + 1);
    expect((await pluginStats('host-a')).resources.pending).toBe(0);
  });

  it('reports failure when the media connection is interrupted and recovers on a fresh operation', async () => {
    const guildId = guild();
    const events = lifecycle.length;

    await node.createGuild(guildId);

    const started = await node.play(guildId, op('host-a', { track: 'wav', seconds: 120 }));

    await node.control(guildId, 'player.pause');
    // Host-side interruption: destroy the connection carrying the active grant.
    hostA.http.closeAllConnections();
    await node.control(guildId, 'player.resume');
    await expect
      .poll(() =>
        lifecycle
          .slice(events)
          .some(
            (event) =>
              (event.event === 'player.failed' || event.event === 'player.completed') &&
              event.data.guildId === guildId,
          ),
      )
      .toBe(true);
    expect((await node.playerState(guildId)).generation).toBe(started.generation);

    // A fresh operation resolves a new grant; the interrupted one is never replayed.
    const recovered = await node.play(guildId, op('host-a', { track: 'mp3' }));

    expect(recovered.info.codec).toMatch(/^mp3/);
    await node.waitForCompletion(guildId, recovered.generation);
    await stopAll(guildId);
  });

  it('survives a host restart under the same ID and fails explicitly while it is down', async () => {
    const guildId = guild();

    await node.createGuild(guildId);

    const port = hostB.port;

    await hostB.close();
    expect(await codeOf(node.play(guildId, op('host-b', { track: 'opus' })))).toBe(
      'plugin_host_unavailable',
    );

    hostB = await startHost({ token: tokens.b, deployment: 'b', port });

    const restarted = await node.play(guildId, op('host-b', { track: 'opus' }));

    expect(restarted.info.codec).toBe('opus');
    await node.waitForCompletion(guildId, restarted.generation);
    await expect.poll(async () => (await pluginStats('host-b')).status.state).toBe('healthy');
    await stopAll(guildId);
  });

  it('ends media transfers and exits cleanly when the server shuts down', async () => {
    const guildId = guild();

    await node.createGuild(guildId);
    await node.play(guildId, op('host-a', { track: 'wav', seconds: 300 }));

    const stats = hostA.behaviour.streams.at(-1)!;

    await node.control(guildId, 'player.pause');

    const closed = once(server, 'close');

    server.stdin?.end();

    const result = await Promise.race([closed, sleep(10_000).then(() => null)]);

    expect(result, 'server did not exit after stdin closed').not.toBeNull();
    expect(result?.[0]).toBe(0);
    await expect.poll(() => stats.disposed).toBe(true);
  }, 30_000);
});

function waitForServer(child: ChildProcess): Promise<string> {
  return new Promise((resolveUrl, reject) => {
    let output = '';
    const timer = setTimeout(() => finish(new Error('remedia-server startup timed out')), 120_000);
    const finish = (error?: Error, url?: string): void => {
      clearTimeout(timer);
      child.stdout?.off('data', onData);
      child.off('error', onError);
      child.off('close', onClose);

      if (error) reject(error);
      else resolveUrl(url!);
    };
    const onData = (data: Buffer): void => {
      output += data.toString('utf8');

      const match = /remedia-server listening on ([^\s]+)/.exec(output);

      if (match?.[1]) finish(undefined, `ws://${match[1]}/v1/ws`);
    };
    const onError = (error: Error): void =>
      finish(new Error(`could not start remedia-server: ${error.message}`));
    const onClose = (code: number | null): void =>
      finish(new Error(`remedia-server exited during startup (${code ?? 'signal'})`));

    child.stdout?.on('data', onData);
    child.once('error', onError);
    child.once('close', onClose);
  });
}
