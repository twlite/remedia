import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { afterEach, beforeEach, describe, test } from 'vitest';

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
  build: runtime,
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

describe('a fresh Node.js process without FFmpeg loader configuration', () => {
  test('loads the engine, decodes media and reports the selected FFmpeg runtime', () => {
    const result = run(cleanEnvironment(directory));

    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);

    const report = JSON.parse(result.stdout.trim().split('\n').at(-1)!) as {
      source: string;
      directory: string | null;
      libraries: string[];
      codec: string;
      env: string[];
      build: {
        license: string;
        configuration: string;
        capabilities: { decoders: string[]; filters: string[]; protocols: string[] };
      };
    };

    assert.deepEqual(report.env, [], 'the child must have no loader variables');
    assert.equal(report.libraries.length, 5);
    assert.equal(report.directory, null);
    assert.equal(report.source, 'bundled');
    assert.equal(report.codec, 'mp3');
    assert.match(report.build.license, /GPL version 3/);
    assert.ok(report.build.configuration.includes('--enable-gpl'));
    assert.ok(!report.build.configuration.includes('--enable-nonfree'));
    assert.ok(report.build.capabilities.decoders.includes('pcm_s16le'));
    assert.ok(report.build.capabilities.decoders.includes('opus'));
    assert.ok(report.build.capabilities.filters.includes('atempo'));
    assert.ok(report.build.capabilities.filters.includes('aresample'));
    assert.ok(report.build.capabilities.protocols.includes('https'));
  });

  test('ignores external FFmpeg overrides because media libraries are bundled', () => {
    const result = run(
      cleanEnvironment(directory, { REMEDIA_FFMPEG_DIR: join(directory, 'missing') }),
    );

    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    const report = JSON.parse(result.stdout.trim().split('\n').at(-1)!) as { source: string };
    assert.equal(report.source, 'bundled');
  });
});
