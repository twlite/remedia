import { expect, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import { FakeServerNode } from './helpers.js';

it.each([false, true])(
  'preserves old statistics and additive process resources (%s)',
  async (modern) => {
    const server = await FakeServerNode.listen({ id: 'resources' });
    if (modern)
      server.resources = {
        memory: {
          residentBytes: 1000,
          privateResidentBytes: 200,
          committedPrivateBytes: 4000,
          footprintBytes: null,
        },
        cpu: { processPercent: 250, threadCount: 17 },
        collectedAtUnixMs: 123,
        cpuSampleIntervalMs: 1001,
        samplingIntervalMs: 1000,
        collectionDurationMs: 0.5,
        openHandles: 23,
      };
    const client = await RemediaClient.create({
      botUserId: server.botUserId,
      nodes: [
        {
          type: 'remote',
          id: 'resources',
          url: server.url,
          auth: { username: 'bot', password: 'secret' },
        },
      ],
      send() {},
    });
    try {
      const resourceNode = client.nodes.find((candidate) => candidate.id === 'resources');
      expect(resourceNode?.statistics?.resources ?? null).toEqual(server.resources ?? null);
      if (modern) {
        expect(resourceNode?.statistics?.memory.privateResidentBytes).toBe(200);
        expect(resourceNode?.statistics?.memory.committedPrivateBytes).toBe(4000);
        expect(resourceNode?.statistics?.resources?.cpu.processPercent).toBe(250);
      }
    } finally {
      await client.close();
      await server.close();
    }
  },
);
