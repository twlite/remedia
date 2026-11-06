import { Readable } from 'node:stream';
import { expect, it } from 'vitest';
import { RemediaClient, UnsupportedMediaSourceError } from '../lib/index.js';
import { FakeServerNode } from './helpers.js';

it('routes stateless remote probes, normalizes unknowns and rejects remote byte sources', async () => {
  const server = await FakeServerNode.listen({ id: 'probes' });
  const client = await RemediaClient.create({
    botUserId: server.botUserId,
    nodes: ['a', 'b'].map((id) => ({
      type: 'remote' as const,
      id,
      url: server.url,
      auth: { username: 'bot', password: 'secret' },
    })),
    send: () => undefined,
  });
  try {
    await expect(client.probe({ url: 'https://example.com/audio' })).rejects.toThrow(/explicit/);
    await expect(client.probe({ file: '/audio.mp3' }, { nodeId: 'missing' })).rejects.toThrow();
    const info = await client.probe({ url: 'https://example.com/audio' }, { nodeId: 'a' });
    expect(info).toEqual({
      streamIndex: 0,
      codec: 'mp3',
      sampleRate: 44100,
      channels: 2,
      channelLayout: null,
      durationMs: null,
      bitRate: null,
      metadata: { title: 'Probe fixture' },
    });
    await expect(
      client.probe({ buffer: new Uint8Array([1]) }, { nodeId: 'a' }),
    ).rejects.toBeInstanceOf(UnsupportedMediaSourceError);
    await expect(
      client.probe({ stream: Readable.from([]) }, { nodeId: 'a' }),
    ).rejects.toBeInstanceOf(UnsupportedMediaSourceError);
    await expect(
      client.probe({ file: '/audio' }, { nodeId: 'b', signal: AbortSignal.abort() }),
    ).rejects.toMatchObject({ name: 'AbortError' });
    expect(client.assignments).toEqual([]);
    expect(server.playerPlayRequests).toBe(0);
  } finally {
    await client.close();
    await server.close();
  }
});

it('propagates cancellation of an in-flight remote probe by request ID', async () => {
  const server = await FakeServerNode.listen({ id: 'probe-cancel' });
  let started!: () => void;
  let release!: () => void;
  const entered = new Promise<void>((resolve) => {
    started = resolve;
  });
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.probeGate = () => {
    started();
    return gate;
  };
  const client = await RemediaClient.create({
    botUserId: server.botUserId,
    nodes: [
      {
        type: 'remote',
        id: 'only',
        url: server.url,
        auth: { username: 'bot', password: 'secret' },
      },
    ],
    send: () => undefined,
  });
  try {
    const controller = new AbortController();
    const operation = client.probe(
      { url: 'https://example.com/audio' },
      { signal: controller.signal },
    );
    await entered;
    controller.abort();
    await expect(operation).rejects.toMatchObject({ name: 'AbortError' });
    await expect.poll(() => server.probeCancelRequests).toBe(1);
    expect(client.assignments).toEqual([]);
  } finally {
    release();
    await client.close();
    await server.close();
  }
});
