import { describe, expect, it, vi } from 'vitest';

vi.mock('@remedia/node', () => ({
  Remedia: {
    create: () =>
      Promise.reject(
        new Error(
          'Remedia could not load a compatible FFmpeg 9 runtime.\nInstall FFmpeg with --ffmpeg-only.',
        ),
      ),
  },
}));

describe('embedded node load failures', () => {
  it('keeps the native package diagnostics in the reported error', async () => {
    const { EmbeddedNodeAdapter } = await import('../lib/embedded-node.js');
    const statuses: Array<string | null> = [];
    const adapter = new EmbeddedNodeAdapter(
      { id: 'embedded' },
      () => Promise.resolve(),
      (event) => {
        statuses.push(event.error?.message ?? null);
      },
    );

    await expect(adapter.start()).rejects.toThrow(
      /embedded @remedia\/node could not be loaded: Remedia could not load a compatible FFmpeg 9 runtime\./,
    );
    expect(adapter.status).toBe('disconnected');
    expect(statuses.at(-1)).toContain('--ffmpeg-only');
  });
});
