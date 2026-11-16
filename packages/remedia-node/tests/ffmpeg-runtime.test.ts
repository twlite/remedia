import { describe, expect, test } from 'vitest';
import { getFFmpegRuntime } from '../lib/native-binding.js';

describe('bundled FFmpeg metadata', () => {
  test('reports the linked libraries and capabilities without library discovery', () => {
    const info = getFFmpegRuntime();

    expect(info.source).toBe('bundled');
    expect(info.directory).toBeNull();
    expect(info.considered).toEqual([]);
    expect(info.version).toBeTruthy();
    expect(info.license).toMatch(/GPL version 3 or later/);
    expect(info.configuration).toContain('--enable-gpl');
    expect(info.configuration).toContain('--enable-version3');
    expect(info.configuration).not.toContain('--enable-nonfree');
    expect(info.remediaVersion).toBeTruthy();
    expect(info.remediaCommit).toBeTruthy();
    expect(info.libraries.map((library) => library.name).sort()).toEqual(
      ['avcodec', 'avfilter', 'avformat', 'avutil', 'swresample'].sort(),
    );

    for (const library of info.libraries) {
      expect(library.path).toBe('');
      expect(library.version).toMatch(/^\d+\.\d+\.\d+$/);
      expect(library.major).toBe(Number(library.version.split('.')[0]));
    }

    expect(info.capabilities.decoders).toContain('opus');
    expect(info.capabilities.filters).toContain('atempo');
    expect(info.capabilities.filters).toContain('aresample');
    expect(info.capabilities.protocols).toContain('https');
  });
});
