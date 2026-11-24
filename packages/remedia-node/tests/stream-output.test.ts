import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { afterEach, expect, it } from 'vitest';
import { Remedia } from '../lib/index.js';
import { makeWave } from './helpers.js';

const engines: Remedia[] = [];
afterEach(async () => {
  for (const engine of engines.splice(0)) await engine.close();
});
async function setup() {
  const engine = await Remedia.create();
  engines.push(engine);
  return { engine, session: engine.createPlaybackSession() };
}

it.each(['s16le', 'f32le'] as const)(
  'streams exact normalized PCM duration as %s and completes',
  async (format) => {
    const { session } = await setup();
    const stream = await session.playStream({ buffer: makeWave(0.25) }, { format });
    const chunks: Buffer[] = [];
    for await (const chunk of stream) chunks.push(chunk);
    const pcm = Buffer.concat(chunks);
    expect(pcm.length).toBe(12_000 * 2 * (format === 's16le' ? 2 : 4));
    expect(pcm.some((byte) => byte !== 0)).toBe(true);
    expect(await session.wait()).toBe('completed');
  },
);

it('preserves Opus boundaries and media timeline including a partial final packet', async () => {
  const { session } = await setup();
  const stream = await session.playStream(
    { buffer: makeWave(0.255) },
    { format: 'opus', opus: { complexity: 3 } },
  );
  let sample = 0;
  for await (const packet of stream) {
    expect(Buffer.isBuffer(packet.data)).toBe(true);
    expect(packet.startSample).toBe(sample);
    expect(packet.samplesPerChannel).toBeGreaterThanOrEqual(packet.mediaSamplesPerChannel);
    sample += packet.mediaSamplesPerChannel;
  }
  expect(sample).toBe(12_240);
});

it('stalled consumers allow pause, seek, resume and stop without reading', async () => {
  const { session } = await setup();
  const stream = await session.playStream({ buffer: makeWave(30) }, { format: 's16le' });
  await delay(40);
  expect(session.position.submittedMs).toBeLessThan(2000);
  await session.pause();
  await session.seek(5000);
  await session.resume();
  await session.stop();
  stream.destroy();
});

it('destroying an old stream cannot cancel subsequent playback', async () => {
  const { session } = await setup();
  const old = await session.playStream({ buffer: makeWave(1) }, { format: 'opus' });
  await session.stop();
  const next = await session.playStream({ buffer: makeWave(0.1) }, { format: 'f32le' });
  old.destroy();
  for await (const chunk of next) {
    expect(chunk.length).toBeGreaterThan(0);
  }
  expect(await session.wait()).toBe('completed');
});

it('destroy unblocks production and supports repeated playback', async () => {
  const { session } = await setup();
  for (let attempt = 0; attempt < 10; attempt++) {
    const stream = await session.playStream({ buffer: makeWave(3) }, { format: 's16le' });
    const closed = once(stream, 'close');
    stream.destroy();
    await closed;
    expect(session.state).toBe('stopped');
  }
});

it('shutdown wakes a pending reader', async () => {
  const { engine, session } = await setup();
  const stream = await session.playStream(
    { buffer: makeWave(30) },
    { format: 'opus', pacing: 'realtime' },
  );
  const pending = stream[Symbol.asyncIterator]().next();
  await engine.close();
  await pending;
  stream.destroy();
});

it('applies filters and master volume before PCM serialization', async () => {
  const { session } = await setup();
  await session.filters.set('volume=0');
  await session.setVolume(0.5);
  const stream = await session.playStream({ buffer: makeWave(0.1) }, { format: 'f32le' });
  for await (const chunk of stream) {
    for (let offset = 0; offset < chunk.length; offset += 4)
      expect(chunk.readFloatLE(offset) === 0).toBe(true);
  }
});

it('paces the output sample clock', async () => {
  const { session } = await setup();
  const start = performance.now();
  const stream = await session.playStream(
    { buffer: makeWave(0.25) },
    { format: 'opus', pacing: 'realtime' },
  );
  for await (const packet of stream) expect(packet.data.length).toBeGreaterThan(0);
  expect(performance.now() - start).toBeGreaterThanOrEqual(200);
});

it('rejects malformed input without leaving a playback active', async () => {
  const { session } = await setup();
  await expect(
    session.playStream({ buffer: Buffer.from('invalid') }, { format: 'opus' }),
  ).rejects.toThrow();
  const next = await session.playStream({ buffer: makeWave(0.1) }, { format: 's16le' });
  for await (const chunk of next) expect(chunk.length).toBeGreaterThan(0);
});

it('continues one output timeline across staged crossfade promotion', async () => {
  const { session } = await setup();
  session.configureCrossfade({ enabled: true, durationMs: 1000, prepareBeforeMs: 5000 });
  const stream = await session.playStream({ buffer: makeWave(3) }, { format: 'opus' });
  const ready = once(session, 'stageReady');
  await session.stageNext({ buffer: makeWave(1) }, { entryId: 'next', queueRevision: 1 });
  await ready;
  let sample = 0;
  for await (const packet of stream) {
    expect(packet.startSample).toBe(sample);
    sample += packet.mediaSamplesPerChannel;
  }
  expect(sample).toBeGreaterThanOrEqual(144000);
  expect(sample).toBeLessThanOrEqual(192000);
});

it('seeking flushes unread native Opus packets before restarting the sample timeline', async () => {
  const { session } = await setup();
  const stream = await session.playStream({ buffer: makeWave(10) }, { format: 'opus' });
  await session.pause();
  await session.seek(5000);
  await session.resume();
  const iterator = stream[Symbol.asyncIterator]();
  const first = await iterator.next();
  expect(first.done).toBe(false);
  expect(first.value?.startSample).toBeGreaterThanOrEqual(240000);
  await iterator.return?.();
});

it('forwards eligible source Opus payloads without transcoding', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(
    new URL(
      '../../../crates/remedia-core/tests/fixtures/generated-opus-passthrough-long.ogg',
      import.meta.url,
    ),
  );
  const packets: Buffer[] = [];
  let partial: Buffer[] = [];
  for (let offset = 0; offset < source.length;) {
    expect(source.toString('ascii', offset, offset + 4)).toBe('OggS');
    const count = source[offset + 26]!;
    let payload = offset + 27 + count;
    for (let index = 0; index < count; index++) {
      const length = source[offset + 27 + index]!;
      partial.push(source.subarray(payload, payload + length));
      payload += length;
      if (length < 255) {
        packets.push(Buffer.concat(partial));
        partial = [];
      }
    }
    offset = payload;
  }
  const { session } = await setup();
  const stream = await session.playStream({ buffer: source }, { format: 'opus' });
  let forwarded = 0;
  let sample = 0;
  for await (const packet of stream) {
    expect(packet.startSample).toBe(sample);
    sample += packet.mediaSamplesPerChannel;
    if (packets.some((sourcePacket) => sourcePacket.equals(packet.data))) forwarded++;
  }
  expect(forwarded).toBeGreaterThan(5);
});

it('natural EOF never races stream auto-destruction into stopping a completed generation', async () => {
  const { session } = await setup();
  for (let attempt = 0; attempt < 30; attempt++) {
    const stream = await session.playStream({ buffer: makeWave(0.02) }, { format: 's16le' });
    for await (const chunk of stream) expect(chunk.length).toBeGreaterThan(0);
    expect(await session.wait()).toBe('completed');
  }
});
