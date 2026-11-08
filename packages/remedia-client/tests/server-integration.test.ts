import { createServer, type ServerResponse, type Server } from 'node:http';
import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { once } from 'node:events';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, describe, expect, it } from 'vitest';
import { RemoteNodeAdapter, RemoteNodeEvent } from '../lib/remote-node.js';

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const fixture = resolve(repositoryRoot, 'crates/remedia-core/tests/fixtures/generated-tone.mp3');

describe('SDK against the Rust WebSocket server', () => {
  let server: ChildProcess | undefined;
  let configDirectory: string | undefined;
  let node: RemoteNodeAdapter | undefined;
  let mediaServer: Server | undefined;

  afterEach(async () => {
    await node?.close();
    mediaServer?.closeAllConnections();
    if (mediaServer)
      await new Promise<void>((resolveClose) => mediaServer!.close(() => resolveClose()));
    mediaServer = undefined;
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

  it.each([false, true])(
    'authenticates and reconciles playback with crossfade policy %s',
    async (crossfade) => {
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
            allowedFileRoots: [
              resolve(repositoryRoot, 'crates/remedia-core/tests/fixtures'),
              mediaDirectory,
            ],
            allowedUrlHosts: ['127.0.0.1'],
            allowHttp: true,
            allowHttps: true,
            blockPrivateNetworks: false,
          },
          audio: { crossfade: { enabled: crossfade } },
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
      expect(node.snapshot.capabilities?.crossfade.available).toBe(crossfade);
      if (!crossfade) {
        await expect(node.createGuild(guildId, { enabled: true })).rejects.toThrow();
      }
      await node.createGuild(guildId, crossfade ? { enabled: true, durationMs: 1000 } : undefined);
      if (crossfade) {
        const wave = silentWave(20);
        const incoming = resolve(mediaDirectory, 'next.wav');
        await writeFile(incoming, wave);
        let response: ServerResponse | undefined;
        mediaServer = createServer((_request, outgoing) => {
          response = outgoing;
          outgoing.writeHead(200, { 'content-type': 'audio/wav', 'content-length': wave.length });
          outgoing.write(wave.subarray(0, 512 * 1024));
        });
        mediaServer.listen(0, '127.0.0.1');
        await once(mediaServer, 'listening');
        const address = mediaServer.address();
        if (!address || typeof address === 'string') throw new Error('media server address');
        const events: Array<{ event: string; data: Record<string, unknown> }> = [];
        node.on(RemoteNodeEvent.lifecycle, (event) => events.push(event));
        const first = await node.play(guildId, {
          type: 'url',
          url: `http://127.0.0.1:${address.port}/current.wav`,
        });
        await node.control(guildId, 'player.stage.next', {
          source: { type: 'file', path: incoming },
          expectedGeneration: first.generation,
          entryId: 'next',
          queueRevision: 1,
          sourceRevision: 0,
        });
        await expect
          .poll(
            async () =>
              (await node!.request<{ state: string }>('player.stage.state', { guildId })).state,
          )
          .toBe('ready');
        response!.end(wave.subarray(512 * 1024));
        await expect
          .poll(() => events.filter((e) => e.event === 'player.completed').length)
          .toBe(2);
        const completions = events.filter((e) => e.event === 'player.completed');
        expect(completions[0]?.data.continuation).toEqual({
          type: 'promoted',
          generation: first.generation + 1,
          entryId: 'next',
        });
        expect(events.filter((e) => e.event === 'player.started')).toHaveLength(2);
        return;
      }
      const started = await node.play(guildId, { type: 'file', path: fixture });
      const completion = await node.waitForCompletion(guildId, started.generation);
      expect(completion).toEqual({ generation: started.generation, state: 'completed' });
      expect((await node.playerState(guildId)).generation).toBe(started.generation);
    },
    120_000,
  );
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

function silentWave(seconds: number): Buffer {
  const bytes = seconds * 48000 * 2;
  const wave = Buffer.alloc(44 + bytes);
  wave.write('RIFF');
  wave.writeUInt32LE(bytes + 36, 4);
  wave.write('WAVEfmt ', 8);
  wave.writeUInt32LE(16, 16);
  wave.writeUInt16LE(1, 20);
  wave.writeUInt16LE(1, 22);
  wave.writeUInt32LE(48000, 24);
  wave.writeUInt32LE(96000, 28);
  wave.writeUInt16LE(2, 32);
  wave.writeUInt16LE(16, 34);
  wave.write('data', 36);
  wave.writeUInt32LE(bytes, 40);
  return wave;
}
