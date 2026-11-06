import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { once } from 'node:events';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, describe, expect, it } from 'vitest';
import { RemoteNodeAdapter } from '../lib/remote-node.js';

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const fixture = resolve(repositoryRoot, 'crates/remedia-core/tests/fixtures/generated-tone.mp3');

describe('SDK against the Rust WebSocket server', () => {
  let server: ChildProcess | undefined;
  let configDirectory: string | undefined;
  let node: RemoteNodeAdapter | undefined;

  afterEach(async () => {
    await node?.close();
    node = undefined;
    if (server && server.exitCode === null) {
      const closed = once(server, 'close');
      server.kill('SIGTERM');
      await Promise.race([closed, new Promise((resolveWait) => setTimeout(resolveWait, 3_000))]);
    }
    server = undefined;
    if (configDirectory) await rm(configDirectory, { recursive: true, force: true });
    configDirectory = undefined;
  });

  it('authenticates, allocates a guild, plays local media, and reconciles generation completion', async () => {
    configDirectory = await mkdtemp(resolve(tmpdir(), 'remedia-client-server-'));
    const mediaDirectory = resolve(configDirectory, 'media');
    const { mkdir } = await import('node:fs/promises');
    await mkdir(mediaDirectory);
    const configPath = resolve(configDirectory, 'remedia.config.json');
    await writeFile(
      configPath,
      JSON.stringify({
        bind: { host: '127.0.0.1', port: 0 },
        users: [
          {
            username: 'sdk-test',
            password: 'sdk-test-password',
          },
        ],
        sessionResumptionTimeoutSeconds: 10,
        limits: {
          maxConnections: 8,
          maxClientSessions: 4,
          maxTotalPlayers: 8,
          maxPlayersPerClient: 4,
          maxMessageBytes: 65_536,
          requestTimeoutMs: 10_000,
          maxInFlightRequests: 8,
          maxRequestsPerSecond: 60,
          eventBuffer: 32,
        },
        media: {
          allowedFileRoots: [resolve(repositoryRoot, 'crates/remedia-core/tests/fixtures')],
          allowedUrlHosts: [],
          allowHttp: true,
          allowHttps: true,
          blockPrivateNetworks: true,
        },
        observability: { console: false },
      }),
    );

    const serverBinary = process.env.REMEDIA_SERVER_BINARY;
    server = spawn(
      serverBinary ?? 'cargo',
      serverBinary
        ? ['--config', configPath]
        : ['run', '--quiet', '-p', 'remedia-server', '--', '--config', configPath],
      { cwd: repositoryRoot, stdio: ['ignore', 'pipe', 'pipe'] },
    );
    server.stderr?.resume();
    const url = await waitForServer(server);
    node = new RemoteNodeAdapter({
      botUserId: '100000000000000001',
      config: {
        type: 'remote',
        id: 'actual-server',
        url: url.replace('ws://', 'remedia://'),
        auth: { username: 'sdk-test', password: 'sdk-test-password' },
      },
      requestTimeoutMs: 10_000,
      maxPendingRequests: 16,
    });
    await node.start();
    const probed = await node.probe({ file: fixture });
    expect(probed.codec).toMatch(/^mp3/);
    expect(probed.durationMs).toBeGreaterThan(0);
    const guildId = '200000000000000901';
    await node.createGuild(guildId);
    const started = await node.play(guildId, { type: 'file', path: fixture });
    const completion = await node.waitForCompletion(guildId, started.generation);
    expect(completion).toEqual({ generation: started.generation, state: 'completed' });
    expect((await node.playerState(guildId)).generation).toBe(started.generation);
  }, 120_000);
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
