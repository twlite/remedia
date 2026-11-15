import { once } from 'node:events';
import { createServer, type Server } from 'node:http';
import { afterEach, expect, it } from 'vitest';
import { Remedia } from '../lib/index.js';
import { makeWave } from './helpers.js';

/** An origin that ignores `Range` and paces its body, so a track plays at network speed. */
async function createRangelessServer(
  wave: Buffer,
  chunkBytes: number,
  chunkDelayMs: number,
  refuseFrom = Number.POSITIVE_INFINITY,
) {
  let requests = 0;
  const sockets = new Set<import('node:net').Socket>();
  const server: Server = createServer((_request, response) => {
    requests += 1;
    if (requests > refuseFrom) {
      response.writeHead(503, { 'content-length': 0 });
      response.end();
      return;
    }
    response.writeHead(200, { 'content-length': wave.length, 'content-type': 'audio/wav' });
    let offset = 0;
    const timer = setInterval(() => {
      if (response.destroyed || offset >= wave.length) {
        clearInterval(timer);
        if (!response.destroyed) response.end();
        return;
      }
      response.write(wave.subarray(offset, offset + chunkBytes));
      offset += chunkBytes;
    }, chunkDelayMs);
    response.on('close', () => clearInterval(timer));
  });
  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('test server did not bind a port');

  return {
    url: `http://127.0.0.1:${address.port}/track.wav`,
    get requests() {
      return requests;
    },
    async close() {
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

it('is off by default and is switched with an option or at any time', async () => {
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());

  const plain = engine.createPlaybackSession();
  expect(plain.fallbackSeeking).toBe(false);

  const enabled = engine.createPlaybackSession({ enableFallbackSeeking: true });
  expect(enabled.fallbackSeeking).toBe(true);

  plain.configureFallbackSeeking(true);
  expect(plain.fallbackSeeking).toBe(true);
  plain.configureFallbackSeeking(false);
  expect(plain.fallbackSeeking).toBe(false);

  expect(engine.createPlaybackSession({ enableFallbackSeeking: false }).fallbackSeeking).toBe(
    false,
  );
  expect(() => plain.configureFallbackSeeking('yes' as unknown as boolean)).toThrow(TypeError);
});

it('replaces a source whose origin ignores Range and keeps playing', async () => {
  const wave = makeWave(8);
  const origin = await createRangelessServer(wave, 8 * 1024, 20);
  cleanups.push(() => origin.close());
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession({ enableFallbackSeeking: true });
  const seeked = once(session, 'playbackSeeked');

  await session.play({ url: origin.url });
  await expect.poll(() => session.position.mediaMs ?? 0, { timeout: 5_000 }).toBeGreaterThan(2_000);
  expect(origin.requests).toBe(1);

  // Rewinding a source that cannot be repositioned opens the track a second time.
  await session.seek(500);
  await seeked;
  expect(origin.requests).toBe(2);
  expect(session.state).toBe('playing');

  // The new source carries on from just after the target and plays to the end.
  const completed = once(session, 'playbackCompleted');
  await completed;
  expect(session.state).toBe('completed');
  expect(origin.requests).toBe(2);
}, 30_000);

it('leaves the playing source untouched when the track cannot be reopened', async () => {
  const wave = makeWave(6);
  // The first connection plays; every later one is refused.
  const origin = await createRangelessServer(wave, 8 * 1024, 20, 1);
  cleanups.push(() => origin.close());
  const engine = await Remedia.create();
  cleanups.push(() => engine.close());
  const session = engine.createPlaybackSession({ enableFallbackSeeking: true });

  await session.play({ url: origin.url });
  await expect.poll(() => session.position.mediaMs ?? 0, { timeout: 5_000 }).toBeGreaterThan(1_500);

  await expect(session.seek(200)).rejects.toThrow();
  expect(origin.requests).toBe(2);
  expect(session.state).not.toBe('failed');

  // The original stream was never interrupted and reaches its natural end.
  await once(session, 'playbackCompleted');
  expect(session.state).toBe('completed');
  expect(session.error).toBeNull();
}, 30_000);
