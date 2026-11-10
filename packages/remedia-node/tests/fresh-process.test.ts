import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { afterEach, beforeEach, describe, test } from 'vitest';

import { FFMPEG_LIBRARIES, layoutDirectories, libraryFileName } from '../lib/ffmpeg-runtime.js';

const distEntry = fileURLToPath(new URL('../dist/index.js', import.meta.url));
const fixture = fileURLToPath(
  new URL('../../../crates/remedia-core/tests/fixtures/generated-tone.mp3', import.meta.url),
);

/**
 * Environment for a fresh Node process that has no FFmpeg loader configuration at all: none of
 * LD_LIBRARY_PATH, DYLD_*, REMEDIA_* and a PATH reduced to the operating system's own directories.
 */
function cleanEnvironment(home: string, extra: Record<string, string> = {}): NodeJS.ProcessEnv {
  const windowsRoot = process.env.SystemRoot ?? 'C:\\Windows';

  return process.platform === 'win32'
    ? {
        SystemRoot: windowsRoot,
        windir: windowsRoot,
        PATH: `${windowsRoot}\\System32;${windowsRoot}`,
        USERPROFILE: home,
        TEMP: process.env.TEMP ?? tmpdir(),
        TMP: process.env.TMP ?? tmpdir(),
        ...extra,
      }
    : {
        HOME: home,
        PATH: '/usr/bin:/bin',
        TMPDIR: tmpdir(),
        ...extra,
      };
}

const script = `
const [entry, fixture] = process.argv.slice(2);
const { Remedia, getFfmpegRuntime } = await import(entry);
const runtime = getFfmpegRuntime();
const remedia = await Remedia.create();
const info = await remedia.probe({ file: fixture });
const playback = remedia.createPlaybackSession();
await playback.play({ file: fixture });
await playback.wait();
await playback.dispose();
await remedia.close();
console.log(JSON.stringify({
  source: runtime.source,
  directory: runtime.directory,
  libraries: runtime.libraries.map((library) => library.name + ' ' + library.version),
  codec: info.codec,
  env: Object.keys(process.env).filter((key) => /^(LD_|DYLD_|REMEDIA_)/.test(key)),
}));
`;

let directory: string;

beforeEach(() => {
  directory = mkdtempSync(join(tmpdir(), 'remedia fresh process '));
});

afterEach(() => {
  rmSync(directory, { recursive: true, force: true });
});

function run(env: NodeJS.ProcessEnv) {
  const file = join(directory, 'child.mjs');

  writeFileSync(file, script);

  // Node's ESM loader requires a file:// URL for absolute paths on Windows.
  return spawnSync(process.execPath, [file, pathToFileURL(distEntry).href, fixture], {
    encoding: 'utf8',
    env,
    timeout: 60_000,
  });
}

function hasManagedFfmpeg(home: string): boolean {
  const prefix = join(home, '.remedia', 'ffmpeg9');

  return layoutDirectories(prefix, process.platform).some((layout) =>
    FFMPEG_LIBRARIES.every(({ name, major }) =>
      existsSync(join(layout, libraryFileName(name, major, process.platform))),
    ),
  );
}

describe('a fresh Node.js process without FFmpeg loader configuration', () => {
  test('loads the engine, decodes media and reports the selected FFmpeg runtime', (context) => {
    // Uses the real home directory, so it exercises whatever installation the machine has:
    // the system's, or the installer-managed ~/.remedia/ffmpeg9.
    const result = run(cleanEnvironment(process.env.REMEDIA_TEST_HOME ?? homedir()));

    // Some machines only have FFmpeg reachable through loader variables (for example a build
    // environment's LD_LIBRARY_PATH), which this test deliberately removes. CI that installs
    // FFmpeg for this purpose sets REMEDIA_REQUIRE_FRESH_PROCESS so that a skip becomes a failure.
    if (
      result.status !== 0 &&
      !process.env.REMEDIA_REQUIRE_FRESH_PROCESS &&
      /FfmpegLoadError/.test(result.stderr) &&
      !/abi-mismatch|architecture-mismatch|unresolved-dependency/.test(result.stderr)
    ) {
      context.skip();
    }

    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);

    const report = JSON.parse(result.stdout.trim().split('\n').at(-1)!) as {
      source: string;
      directory: string;
      libraries: string[];
      codec: string;
      env: string[];
    };

    assert.deepEqual(report.env, [], 'the child must have no loader variables');
    assert.equal(report.libraries.length, 5);
    assert.ok(report.directory.length > 0);
    assert.equal(report.codec, 'mp3');

    // CI sets this to prove the installer-managed prefix is what a default setup resolves to.
    if (process.env.REMEDIA_EXPECT_FFMPEG_SOURCE) {
      assert.equal(report.source, process.env.REMEDIA_EXPECT_FFMPEG_SOURCE);
    }
  });

  test.skipIf(!hasManagedFfmpeg(process.env.REMEDIA_TEST_HOME ?? homedir()))(
    'REMEDIA_FFMPEG_DIR selects the installation explicitly',
    () => {
      const home = process.env.REMEDIA_TEST_HOME ?? homedir();
      const result = run(
        cleanEnvironment(home, { REMEDIA_FFMPEG_DIR: join(home, '.remedia', 'ffmpeg9') }),
      );

      assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);

      const report = JSON.parse(result.stdout.trim().split('\n').at(-1)!) as { source: string };

      assert.equal(report.source, 'explicit');
    },
  );

  test('an invalid REMEDIA_FFMPEG_DIR fails the import with an actionable message', () => {
    const result = run(
      cleanEnvironment(homedir(), { REMEDIA_FFMPEG_DIR: join(directory, 'nope') }),
    );

    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /REMEDIA_FFMPEG_DIR does not point to a loadable FFmpeg 9/);
    assert.match(result.stderr, /not-found/);
    assert.match(result.stderr, /--ffmpeg-only|-FFmpegOnly/);
  });

  test('an installation with a different ABI major is rejected, never silently replaced', () => {
    const wrong = join(directory, 'ffmpeg 7');
    const library = layoutDirectories(wrong, process.platform)[0]!;

    mkdirSync(library, { recursive: true });
    writeFileSync(join(library, libraryFileName('avcodec', 61, process.platform)), 'not ffmpeg 9');

    const result = run(cleanEnvironment(homedir(), { REMEDIA_FFMPEG_DIR: wrong }));

    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /abi-mismatch/);
    assert.match(result.stderr, /libavcodec ABI 61 \(requires 63\)/);
  });
});
