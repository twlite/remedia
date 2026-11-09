import { expect, test } from 'vitest';
import { Remedia } from '../lib/index.js';

test('native snapshots describe the shared process and preserve unavailable memory counters', async () => {
  const first = await Remedia.create();
  const second = await Remedia.create();
  try {
    let resources = first.processResources();
    const deadline = Date.now() + 5000;
    while (resources.collectedAtUnixMs === null) {
      expect(Date.now()).toBeLessThan(deadline);
      await new Promise((resolve) => setTimeout(resolve, 5));
      resources = first.processResources();
    }
    expect(second.processResources().collectedAtUnixMs).toBe(resources.collectedAtUnixMs);
    expect(resources.cpu.threadCount).toBeGreaterThan(0);
    expect(resources.memory.residentBytes).toBeGreaterThan(0);
    expect(resources.samplingIntervalMs).toBe(1000);
    if (process.platform === 'darwin') {
      expect(resources.memory.footprintBytes).toBeGreaterThan(0);
      expect(resources.memory.privateResidentBytes).toBeNull();
      expect(resources.memory.committedPrivateBytes).toBeNull();
    }
  } finally {
    await first.close();
    await second.close();
  }
});
