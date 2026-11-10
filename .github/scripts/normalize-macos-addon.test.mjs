import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { ffmpegAbi } from './ffmpeg-abi.mjs';
import {
  FALLBACK_RPATHS,
  needsRewrite,
  normalizeMacosAddon,
  parseDependencies,
  parseRpaths,
} from './normalize-macos-addon.mjs';

const otoolDependencies = `addon.node:
\t/Users/ci/target/debug/deps/libnode.dylib (compatibility version 0.0.0, current version 0.0.0)
\t/opt/homebrew/opt/ffmpeg/lib/libavcodec.63.dylib (compatibility version 63.0.0, current version 63.1.102)
\t@rpath/libavutil.61.dylib (compatibility version 61.0.0, current version 61.1.102)
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1359.0.0)
`;

test('parses dependencies without the header line', () => {
  assert.deepEqual(parseDependencies(otoolDependencies), [
    '/Users/ci/target/debug/deps/libnode.dylib',
    '/opt/homebrew/opt/ffmpeg/lib/libavcodec.63.dylib',
    '@rpath/libavutil.61.dylib',
    '/usr/lib/libSystem.B.dylib',
  ]);
});

test('parses LC_RPATH entries, including paths with spaces', () => {
  const output = `Load command 12
          cmd LC_RPATH
      cmdsize 48
         path /Users/a b/lib (offset 12)
Load command 13
          cmd LC_UUID
`;

  assert.deepEqual(parseRpaths(output), ['/Users/a b/lib']);
});

test('only absolute FFmpeg dylib dependencies are rewritten', () => {
  assert.equal(needsRewrite('/opt/homebrew/opt/ffmpeg/lib/libavcodec.63.dylib'), true);
  assert.equal(needsRewrite('/usr/local/opt/ffmpeg/lib/libswresample.7.dylib'), true);
  assert.equal(needsRewrite('@rpath/libavcodec.63.dylib'), false);
  assert.equal(needsRewrite('/usr/lib/libSystem.B.dylib'), false);
  assert.equal(needsRewrite('/opt/homebrew/lib/libx264.dylib'), false);
});

test('normalization is a no-op when an addon is already normalized', () => {
  const dependencies = [...ffmpegAbi]
    .map(
      ([name, major]) =>
        `\t@rpath/${name}.${major}.dylib (compatibility version 1.0.0, current version 1.0.0)`,
    )
    .join('\n');
  const commands = [];
  const tools = {
    run(command, args) {
      commands.push([command, ...args]);

      if (command === 'otool' && args[0] === '-L') return `addon.node:\n${dependencies}\n`;
      if (command === 'otool') {
        return FALLBACK_RPATHS.map(
          (path) => `          cmd LC_RPATH\n      cmdsize 48\n         path ${path} (offset 12)\n`,
        ).join('');
      }

      throw new Error(`unexpected ${command}`);
    },
  };

  const result = normalizeMacosAddon('addon.node', tools);

  assert.deepEqual(result.rewritten, []);
  assert.deepEqual(result.addedRpaths, []);
  assert.equal(
    commands.some(([command]) => command === 'codesign'),
    false,
  );
});

test(
  'rewrites a real Mach-O image so preloaded @rpath libraries are reused',
  { skip: process.platform !== 'darwin' },
  () => {
    const directory = mkdtempSync(join(tmpdir(), 'remedia normalize '));

    try {
      const libraries = [...ffmpegAbi].map(([name, major]) => {
        const source = join(directory, `${name}.c`);
        const output = join(directory, `${name}.${major}.dylib`);

        writeFileSync(source, `int ${name}_marker(void) { return ${major}; }\n`);
        execFileSync('cc', ['-dynamiclib', '-install_name', output, source, '-o', output]);

        return output;
      });
      const addonSource = join(directory, 'addon.c');
      const addon = join(directory, 'addon.node');

      writeFileSync(addonSource, 'int addon_marker(void) { return 1; }\n');
      execFileSync('cc', ['-dynamiclib', addonSource, ...libraries, '-o', addon]);

      const before = parseDependencies(execFileSync('otool', ['-L', addon], { encoding: 'utf8' }));

      assert.ok(
        before.some(
          (dependency) =>
            dependency.endsWith('libavcodec.63.dylib') && !dependency.startsWith('@rpath/'),
        ),
      );

      const result = normalizeMacosAddon(addon);
      const after = parseDependencies(execFileSync('otool', ['-L', addon], { encoding: 'utf8' }));

      assert.equal(result.rewritten.length, ffmpegAbi.size);

      for (const [name, major] of ffmpegAbi)
        assert.ok(after.includes(`@rpath/${name}.${major}.dylib`));

      assert.deepEqual(
        parseRpaths(execFileSync('otool', ['-l', addon], { encoding: 'utf8' })).sort(),
        [...FALLBACK_RPATHS].sort(),
      );
      execFileSync('codesign', ['--verify', '--strict', addon]);

      // A second run changes nothing.
      assert.deepEqual(normalizeMacosAddon(addon).rewritten, []);
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  },
);
