import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

const base = '--enable-static --disable-shared --enable-gpl --enable-version3 --disable-autodetect';
for (const [name, configuration, expected] of [
  [
    'accepts the audio/TLS configuration',
    `${base} --enable-openssl --enable-decoder='mp3,pcm_s16le' --enable-filter=atempo,aresample`,
    0,
  ],
  ['rejects nonfree builds', `${base} --enable-nonfree`, 1],
  ['rejects unaudited external libraries', `${base} --enable-libfdk-aac`, 1],
  ['rejects shared builds', base.replace('--disable-shared', '--enable-shared'), 1],
  ['requires GPL version3', base.replace('--enable-version3', ''), 1],
]) {
  test(name, () => {
    const directory = mkdtempSync(join(tmpdir(), 'remedia-static-policy-'));
    try {
      const path = join(directory, 'configuration');
      writeFileSync(path, configuration);
      const result = spawnSync(
        process.execPath,
        ['.github/scripts/audit-ffmpeg-config.mjs', path],
        { encoding: 'utf8' },
      );
      assert.equal(result.status, expected, result.stderr);
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
}
