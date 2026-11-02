import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer, type ServerResponse } from 'node:http';

export async function createWaveFixture(durationSeconds = 0.25): Promise<{
  directory: string;
  path: string;
  wave: Buffer;
  cleanup(): Promise<void>;
}> {
  const directory = await mkdtemp(join(tmpdir(), 'remedia-node-'));
  const path = join(directory, 'tone.wav');
  const wave = makeWave(durationSeconds);
  await writeFile(path, wave);
  return {
    directory,
    path,
    wave,
    async cleanup() {
      await rm(directory, { recursive: true, force: true });
    },
  };
}

export function makeWave(durationSeconds: number, sampleRate = 48_000, channels = 1): Buffer {
  const samplesPerChannel = Math.round(durationSeconds * sampleRate);
  const bytesPerSample = 2;
  const dataLength = samplesPerChannel * channels * bytesPerSample;
  const wave = Buffer.alloc(44 + dataLength);
  wave.write('RIFF', 0);
  wave.writeUInt32LE(36 + dataLength, 4);
  wave.write('WAVE', 8);
  wave.write('fmt ', 12);
  wave.writeUInt32LE(16, 16);
  wave.writeUInt16LE(1, 20);
  wave.writeUInt16LE(channels, 22);
  wave.writeUInt32LE(sampleRate, 24);
  wave.writeUInt32LE(sampleRate * channels * bytesPerSample, 28);
  wave.writeUInt16LE(channels * bytesPerSample, 32);
  wave.writeUInt16LE(bytesPerSample * 8, 34);
  wave.write('data', 36);
  wave.writeUInt32LE(dataLength, 40);

  for (let index = 0; index < samplesPerChannel * channels; index++) {
    const sample = Math.round(Math.sin((index / sampleRate) * Math.PI * 440 * 2) * 12_000);
    wave.writeInt16LE(sample, 44 + index * bytesPerSample);
  }
  return wave;
}

export async function createHeldAudioServer(
  wave: Buffer,
  prefixBytes = 512 * 1024,
): Promise<{
  url: string;
  ready: Promise<void>;
  finish(): void;
  waitForClientClose(): Promise<void>;
  close(): Promise<void>;
}> {
  let response: ServerResponse | undefined;
  let readyResolve: (() => void) | undefined;
  let clientCloseResolve: (() => void) | undefined;
  const ready = new Promise<void>((resolve) => (readyResolve = resolve));
  const clientClosed = new Promise<void>((resolve) => (clientCloseResolve = resolve));
  const server = createServer((_request, outgoing) => {
    response = outgoing;
    outgoing.on('close', () => clientCloseResolve?.());
    outgoing.writeHead(200, {
      'content-length': wave.length,
      'content-type': 'audio/wav',
    });
    outgoing.write(wave.subarray(0, prefixBytes));
    readyResolve?.();
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  if (!address || typeof address === 'string')
    throw new Error('test server did not bind a TCP port');

  return {
    url: `http://127.0.0.1:${address.port}/held.wav`,
    ready,
    finish() {
      response?.end(wave.subarray(prefixBytes));
    },
    async waitForClientClose() {
      let timeout: NodeJS.Timeout | undefined;
      try {
        await Promise.race([
          clientClosed,
          new Promise<never>((_resolve, reject) => {
            timeout = setTimeout(() => reject(new Error('HTTP input was not closed')), 2_000);
          }),
        ]);
      } finally {
        if (timeout) clearTimeout(timeout);
      }
    },
    async close() {
      response?.destroy();
      if (!server.listening) return;
      await new Promise<void>((resolve, reject) => {
        server.close((error) => (error ? reject(error) : resolve()));
      });
    },
  };
}

export async function waitFor<T>(findValue: () => T | undefined, timeoutMs = 2_000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = findValue();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('timed out waiting for the expected value');
}
