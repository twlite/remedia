/** Run with node --expose-gc --experimental-strip-types benchmarks/stream-output.ts.
 * Build @remedia/node first. JSONL rows compare streaming against NullOutput.
 */
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { Remedia } from '../packages/remedia-node/dist/index.js';

const seconds = Number(process.env.STREAM_SECONDS ?? 3);
const repetitions = Number(process.env.STREAM_REPETITIONS ?? 3);
const concurrency = (process.env.STREAM_CONCURRENCY ?? '1,8,32').split(',').map(Number);
if (![seconds, repetitions, ...concurrency].every((value) => Number.isFinite(value) && value > 0))
  throw new Error('benchmark settings must be positive');

function wave(): Buffer {
  const samples = Math.round(seconds * 48000);
  const bytes = samples * 4;
  const data = Buffer.alloc(44 + bytes);
  data.write('RIFF');
  data.writeUInt32LE(36 + bytes, 4);
  data.write('WAVEfmt ', 8);
  data.writeUInt32LE(16, 16);
  data.writeUInt16LE(1, 20);
  data.writeUInt16LE(2, 22);
  data.writeUInt32LE(48000, 24);
  data.writeUInt32LE(192000, 28);
  data.writeUInt16LE(4, 32);
  data.writeUInt16LE(16, 34);
  data.write('data', 36);
  data.writeUInt32LE(bytes, 40);
  for (let i = 0; i < samples; i++) {
    const value = Math.round(Math.sin((i * 440 * 2 * Math.PI) / 48000) * 12000);
    data.writeInt16LE(value, 44 + i * 4);
    data.writeInt16LE(value, 46 + i * 4);
  }
  return data;
}
const input = wave();
const engine = await Remedia.create();
try {
  for (let repeat = 0; repeat < repetitions; repeat++) {
    for (const count of concurrency) {
      for (const mode of ['null', 's16le', 'f32le', 'opus'] as const) {
        for (const consumer of mode === 'null'
          ? (['fast'] as const)
          : (['fast', 'slow', 'stalled'] as const)) {
          global.gc?.();
          const before = process.memoryUsage();
          const cpu = process.cpuUsage();
          const begin = performance.now();
          let chunks = 0,
            bytes = 0,
            peakHeap = before.heapUsed,
            peakExternal = before.external,
            peakRss = before.rss;
          let maxThreads = 0,
            maxDeliveryLagMs = 0;
          const sample = () => {
            const memory = process.memoryUsage();
            peakHeap = Math.max(peakHeap, memory.heapUsed);
            peakExternal = Math.max(peakExternal, memory.external);
            peakRss = Math.max(peakRss, memory.rss);
            maxThreads = Math.max(maxThreads, engine.processResources().cpu.threadCount ?? 0);
          };
          const sampler = setInterval(sample, 25);
          try {
            await Promise.all(
              Array.from({ length: count }, async () => {
                const session = engine.createPlaybackSession();
                try {
                  if (mode === 'null') {
                    await session.play({ buffer: input });
                    await session.wait();
                    return;
                  }
                  const pacing = consumer === 'slow' ? 'realtime' : 'unpaced';
                  const stream = await session.playStream(
                    { buffer: input },
                    { format: mode, pacing },
                  );
                  if (consumer === 'stalled') {
                    await delay(250);
                    sample();
                    await session.stop();
                    stream.destroy();
                    return;
                  }
                  let consumedSamples = 0;
                  const origin = performance.now();
                  for await (const chunk of stream) {
                    chunks++;
                    const payload = Buffer.isBuffer(chunk) ? chunk : chunk.data;
                    bytes += payload.length;
                    if (pacing === 'realtime')
                      maxDeliveryLagMs = Math.max(
                        maxDeliveryLagMs,
                        performance.now() - origin - consumedSamples / 48,
                      );
                    consumedSamples += Buffer.isBuffer(chunk)
                      ? chunk.length / (mode === 's16le' ? 4 : 8)
                      : chunk.mediaSamplesPerChannel;
                    if (consumer === 'slow') await delay(5);
                  }
                } finally {
                  await session.dispose();
                }
              }),
            );
          } finally {
            clearInterval(sampler);
            sample();
          }
          const elapsed = performance.now() - begin;
          const used = process.cpuUsage(cpu);
          global.gc?.();
          console.log(
            JSON.stringify({
              repeat,
              count,
              mode,
              consumer,
              audioSeconds: consumer === 'stalled' ? null : count * seconds,
              wallMs: elapsed,
              cpuMs: (used.user + used.system) / 1000,
              cpuMsPerAudioSecond:
                consumer === 'stalled'
                  ? null
                  : (used.user + used.system) / 1000 / (count * seconds),
              throughputAudioSecondsPerSecond:
                consumer === 'stalled' ? null : (count * seconds) / (elapsed / 1000),
              maxDeliveryLagMs,
              chunks,
              payloadBytes: bytes,
              pcmSerializationBytes: mode === 's16le' || mode === 'f32le' ? bytes : 0,
              before,
              afterGc: process.memoryUsage(),
              peakHeap,
              peakExternal,
              peakRss,
              maxThreads,
              nativeProcessSnapshot: engine.processResources(),
            }),
          );
        }
      }
    }
  }
} finally {
  await engine.close();
}
