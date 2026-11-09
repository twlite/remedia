import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { delimiter, join, resolve } from 'node:path';
import { afterEach, beforeEach, describe, test } from 'vitest';

import {
  FFMPEG_LIBRARIES,
  FfmpegLoadError,
  binaryArchitectures,
  layoutDirectories,
  libraryFileName,
  managedPrefix,
  selectFfmpeg,
  systemDirectories,
  type BootstrapBinding,
  type BootstrapLoadResult,
  type DiscoveryEnvironment,
} from '../lib/ffmpeg-runtime.js';

const packedVersion = (major: number): number => (major << 16) | (1 << 8) | 2;

type Platform = NodeJS.Platform;

let root: string;

beforeEach(() => {
  // The directory name contains spaces to prove paths are never split or unquoted.
  root = mkdtempSync(join(tmpdir(), 'remedia ffmpeg runtime '));
});

afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

/** Create the five FFmpeg 9 libraries for `platform` in `directory`. */
function installLibraries(
  directory: string,
  platform: Platform = process.platform,
  contents: Buffer | string = 'fake',
  skip: readonly string[] = [],
): void {
  mkdirSync(directory, { recursive: true });

  for (const { name, major } of FFMPEG_LIBRARIES) {
    if (skip.includes(name)) continue;

    writeFileSync(join(directory, libraryFileName(name, major, platform)), contents);
  }
}

interface FakeOptions {
  /** ABI major reported by the library, keyed by library name. */
  majors?: Record<string, number>;
  /** Substring of a path whose load fails, with the loader message. */
  failing?: { match: string; message: string; code?: number };
  /** Directory the loader claims a library came from, keyed by library name. */
  resolvedDirectory?: Record<string, string>;
}

interface FakeBootstrap extends BootstrapBinding {
  loaded: string[];
  commits: number;
  rollbacks: number;
}

function fakeBootstrap(options: FakeOptions = {}): FakeBootstrap {
  const fake: FakeBootstrap = {
    loaded: [],
    commits: 0,
    rollbacks: 0,
    bootstrapVersion: () => 1,
    loadLibrary(target: string, versionSymbol?: string | null): BootstrapLoadResult {
      if (options.failing && target.includes(options.failing.match)) {
        return { ok: false, code: options.failing.code ?? 0, message: options.failing.message };
      }

      const name = (versionSymbol ?? '').replace(/_version$/, '');
      const major =
        options.majors?.[name] ??
        FFMPEG_LIBRARIES.find((library) => library.name === name)?.major ??
        0;
      const directory = options.resolvedDirectory?.[name];

      fake.loaded.push(target);

      return {
        ok: true,
        version: packedVersion(major),
        path: directory ? join(directory, libraryFileName(name, major, process.platform)) : target,
      };
    },
    commitLoaded() {
      fake.commits += 1;
    },
    rollbackLoaded() {
      fake.rollbacks += 1;
    },
  };

  return fake;
}

function environment(
  overrides: Partial<DiscoveryEnvironment> & { env?: NodeJS.ProcessEnv } = {},
): DiscoveryEnvironment {
  return {
    platform: process.platform,
    arch: process.arch,
    env: {},
    homeDirectory: join(root, 'home'),
    execPath: join(root, 'node', 'bin', 'node'),
    // Never consult the real machine's libraries; individual tests opt in to directories.
    systemDirectories: [],
    ...overrides,
  };
}

describe('library naming and layouts', () => {
  test("uses each platform's shared-library naming with the FFmpeg 9 ABI majors", () => {
    assert.equal(libraryFileName('avcodec', 63, 'linux'), 'libavcodec.so.63');
    assert.equal(libraryFileName('avutil', 61, 'freebsd'), 'libavutil.so.61');
    assert.equal(libraryFileName('avfilter', 12, 'darwin'), 'libavfilter.12.dylib');
    assert.equal(libraryFileName('swresample', 7, 'win32'), 'swresample-7.dll');
    assert.deepEqual(
      FFMPEG_LIBRARIES.map(({ name, major }) => `${name}:${major}`),
      ['avutil:61', 'swresample:7', 'avcodec:63', 'avformat:63', 'avfilter:12'],
    );
  });

  test("looks for libraries where each platform's conda prefix puts them", () => {
    assert.deepEqual(layoutDirectories('/p', 'linux'), [
      join('/p', 'lib'),
      join('/p', 'lib64'),
      '/p',
    ]);
    assert.deepEqual(layoutDirectories('C:\\p', 'win32'), [
      join('C:\\p', 'Library', 'bin'),
      join('C:\\p', 'bin'),
      'C:\\p',
    ]);
  });

  test("derives the managed prefix from the executing user's home directory", () => {
    assert.equal(
      managedPrefix(environment({ homeDirectory: join(root, 'someone else') })),
      join(root, 'someone else', '.remedia', 'ffmpeg9'),
    );
  });
});

describe('binary architecture detection', () => {
  const write = (name: string, bytes: Buffer): string => {
    const path = join(root, name);

    writeFileSync(path, bytes);

    return path;
  };

  test('reads ELF machine types', () => {
    const elf = (machine: number): Buffer => {
      const buffer = Buffer.alloc(64);

      buffer.writeUInt32BE(0x7f454c46, 0);
      buffer[4] = 2;
      buffer[5] = 1;
      buffer.writeUInt16LE(machine, 18);

      return buffer;
    };

    assert.deepEqual(binaryArchitectures(write('x64.so', elf(62))), ['x64']);
    assert.deepEqual(binaryArchitectures(write('arm64.so', elf(183))), ['arm64']);
    assert.deepEqual(binaryArchitectures(write('arm.so', elf(40))), ['arm']);
  });

  test('reads thin and universal Mach-O files', () => {
    const thin = Buffer.alloc(64);

    thin.writeUInt32LE(0xfeedfacf, 0);
    thin.writeUInt32LE(0x0100000c, 4);
    assert.deepEqual(binaryArchitectures(write('thin.dylib', thin)), ['arm64']);

    const fat = Buffer.alloc(64);

    fat.writeUInt32BE(0xcafebabe, 0);
    fat.writeUInt32BE(2, 4);
    fat.writeUInt32BE(0x01000007, 8);
    fat.writeUInt32BE(0x0100000c, 28);
    assert.deepEqual(binaryArchitectures(write('fat.dylib', fat)), ['x64', 'arm64']);
  });

  test('reads PE machine types', () => {
    const pe = (machine: number): Buffer => {
      const buffer = Buffer.alloc(256);

      buffer[0] = 0x4d;
      buffer[1] = 0x5a;
      buffer.writeUInt32LE(0x80, 0x3c);
      buffer.writeUInt32LE(0x00004550, 0x80);
      buffer.writeUInt16LE(machine, 0x84);

      return buffer;
    };

    assert.deepEqual(binaryArchitectures(write('x64.dll', pe(0x8664))), ['x64']);
    assert.deepEqual(binaryArchitectures(write('arm64.dll', pe(0xaa64))), ['arm64']);
  });

  test('returns null for unknown formats and unreadable files', () => {
    assert.equal(
      binaryArchitectures(write('text.so', Buffer.from('not a binary at all, just text'))),
      null,
    );
    assert.equal(binaryArchitectures(join(root, 'does-not-exist')), null);
  });
});

describe('system library directories', () => {
  test('uses the loader search variables on macOS, ignoring relative entries', () => {
    const directories = systemDirectories(
      environment({
        platform: 'darwin',
        env: {
          DYLD_LIBRARY_PATH: `/custom/one${delimiter}relative/dir`,
          DYLD_FALLBACK_LIBRARY_PATH: '/custom/two',
        },
      }),
    );

    assert.deepEqual(directories.slice(0, 2), [resolve('/custom/one'), resolve('/custom/two')]);
    assert.ok(directories.includes(resolve('/opt/homebrew/opt/ffmpeg/lib')));
    assert.ok(directories.includes(resolve('/usr/local/opt/ffmpeg/lib')));
    assert.ok(!directories.some((directory) => directory.includes('relative')));
  });

  test('uses LD_LIBRARY_PATH and the standard directories on Linux, never the current directory', () => {
    const directories = systemDirectories(
      environment({
        platform: 'linux',
        arch: 'x64',
        env: { LD_LIBRARY_PATH: `/opt/ffmpeg/lib${delimiter}${delimiter}.` },
      }),
    );

    assert.equal(directories[0], resolve('/opt/ffmpeg/lib'));
    assert.ok(directories.includes(resolve('/usr/lib/x86_64-linux-gnu')));
    assert.ok(directories.includes(resolve('/usr/lib64')));
    assert.ok(!directories.includes(process.cwd()));
    assert.equal(new Set(directories).size, directories.length, 'directories are de-duplicated');
  });

  test('uses the Node.js directory and absolute PATH entries on Windows, ignoring relative ones', () => {
    const tools = join(root, 'tools');
    const directories = systemDirectories(
      environment({
        platform: 'win32',
        execPath: join(root, 'node dir', 'node.exe'),
        env: { PATH: `${tools}${delimiter}relative${delimiter}` },
      }),
    );

    assert.deepEqual(directories, [join(root, 'node dir'), tools]);
  });
});

describe('discovery precedence', () => {
  test('REMEDIA_FFMPEG_DIR wins over a usable system and managed installation', () => {
    const system = join(root, 'system lib');
    const explicit = join(root, 'explicit prefix');
    const home = join(root, 'home');

    installLibraries(system);
    installLibraries(layoutDirectories(explicit, process.platform)[0]!);
    installLibraries(
      layoutDirectories(managedPrefix(environment({ homeDirectory: home })), process.platform)[0]!,
    );

    const bootstrap = fakeBootstrap();
    const runtime = selectFfmpeg(
      bootstrap,
      environment({
        env: { REMEDIA_FFMPEG_DIR: explicit },
        systemDirectories: [system],
        homeDirectory: home,
      }),
    );

    assert.equal(runtime.source, 'explicit');
    assert.ok(bootstrap.loaded.every((path) => path.startsWith(explicit)));
    assert.equal(bootstrap.commits, 1);
    assert.deepEqual(
      runtime.libraries.map((library) => library.name),
      FFMPEG_LIBRARIES.map((library) => library.name),
    );
  });

  test('an invalid override fails instead of falling back to a working installation', () => {
    const system = join(root, 'system');
    const home = join(root, 'home');

    installLibraries(system);
    installLibraries(
      layoutDirectories(managedPrefix(environment({ homeDirectory: home })), process.platform)[0]!,
    );

    const bootstrap = fakeBootstrap();

    assert.throws(
      () =>
        selectFfmpeg(
          bootstrap,
          environment({
            env: { REMEDIA_FFMPEG_DIR: join(root, 'missing') },
            systemDirectories: [system],
            homeDirectory: home,
          }),
        ),
      (error: unknown) => {
        assert.ok(error instanceof FfmpegLoadError);
        assert.equal(error.candidates.length, 1);
        assert.equal(error.candidates[0]?.source, 'explicit');
        assert.equal(error.candidates[0]?.reason, 'not-found');
        assert.match(error.message, /REMEDIA_FFMPEG_DIR/);

        return true;
      },
    );
    assert.deepEqual(
      bootstrap.loaded,
      [],
      'no other installation may be loaded after a bad override',
    );
  });

  test('an override that exists but is the wrong ABI is rejected without fallback', () => {
    const explicit = join(root, 'ffmpeg 7');

    mkdirSync(explicit, { recursive: true });
    writeFileSync(join(explicit, libraryFileName('avcodec', 61, process.platform)), 'old');

    const bootstrap = fakeBootstrap();

    assert.throws(
      () => selectFfmpeg(bootstrap, environment({ env: { REMEDIA_FFMPEG_DIR: explicit } })),
      (error: unknown) => {
        assert.ok(error instanceof FfmpegLoadError);
        assert.equal(error.candidates[0]?.reason, 'abi-mismatch');
        assert.match(error.candidates[0]?.detail ?? '', /libavcodec ABI 61 \(requires 63\)/);

        return true;
      },
    );
  });

  test('accepts either the installation prefix or its library directory for the override', () => {
    const prefix = join(root, 'prefix');
    const library = layoutDirectories(prefix, process.platform)[0]!;

    installLibraries(library);

    for (const path of [prefix, library]) {
      const runtime = selectFfmpeg(
        fakeBootstrap(),
        environment({ env: { REMEDIA_FFMPEG_DIR: path } }),
      );

      assert.equal(runtime.source, 'explicit');
      assert.equal(runtime.directory, realpathSync(library));
    }
  });

  test('uses an existing system installation before the managed one', () => {
    const system = join(root, 'system');
    const home = join(root, 'home');

    installLibraries(system);
    installLibraries(
      layoutDirectories(managedPrefix(environment({ homeDirectory: home })), process.platform)[0]!,
    );

    const runtime = selectFfmpeg(
      fakeBootstrap(),
      environment({ systemDirectories: [system], homeDirectory: home }),
    );

    assert.equal(runtime.source, 'system');
    assert.equal(runtime.directory, realpathSync(system));
  });

  test("falls back to ~/.remedia/ffmpeg9 under the executing user's home", () => {
    const home = join(root, 'a user with spaces');
    const library = layoutDirectories(
      managedPrefix(environment({ homeDirectory: home })),
      process.platform,
    )[0]!;

    installLibraries(library);

    const runtime = selectFfmpeg(fakeBootstrap(), environment({ homeDirectory: home }));

    assert.equal(runtime.source, 'managed');
    assert.equal(runtime.directory, realpathSync(library));
    assert.equal(runtime.considered.at(-1)?.outcome, 'selected');
  });

  test('reports every considered path with install and override instructions when nothing loads', () => {
    const home = join(root, 'home');

    assert.throws(
      () => selectFfmpeg(fakeBootstrap(), environment({ homeDirectory: home })),
      (error: unknown) => {
        assert.ok(error instanceof FfmpegLoadError);
        assert.deepEqual(
          error.candidates.map((candidate) => candidate.source),
          ['system', 'managed'],
        );
        assert.match(
          error.message,
          /libavcodec 63, libavformat 63, libavutil 61, libavfilter 12, libswresample 7|libavutil 61.*libavfilter 12/,
        );
        assert.ok(error.message.includes(join(home, '.remedia', 'ffmpeg9')));
        assert.match(error.message, /--ffmpeg-only|-FFmpegOnly/);
        assert.match(error.message, /REMEDIA_FFMPEG_DIR/);

        return true;
      },
    );
  });

  test('explains a system directory that only has another FFmpeg major', () => {
    const system = join(root, 'system with ffmpeg 7');

    mkdirSync(system, { recursive: true });
    writeFileSync(join(system, libraryFileName('avcodec', 61, process.platform)), 'old');

    assert.throws(
      () => selectFfmpeg(fakeBootstrap(), environment({ systemDirectories: [system] })),
      (error: unknown) => {
        assert.ok(error instanceof FfmpegLoadError);

        const report = error.candidates.find((candidate) => candidate.source === 'system');

        assert.equal(report?.reason, 'abi-mismatch');
        assert.match(report?.detail ?? '', /libavcodec ABI 61 \(requires 63\)/);

        return true;
      },
    );
  });

  test('shows the PowerShell installer command for Windows', () => {
    const error = new FfmpegLoadError([], false, 'win32');

    assert.match(error.message, /-FFmpegOnly/);
    assert.match(error.message, /Library\\bin/);
  });
});

describe('compatibility checks', () => {
  const home = (): string => join(root, 'home');
  const managedLibrary = (): string =>
    layoutDirectories(managedPrefix(environment({ homeDirectory: home() })), process.platform)[0]!;

  function failureFor(
    options: FakeOptions,
    prepare: () => void = () => installLibraries(managedLibrary()),
  ) {
    prepare();

    const bootstrap = fakeBootstrap(options);

    try {
      selectFfmpeg(bootstrap, environment({ homeDirectory: home() }));
    } catch (error) {
      assert.ok(error instanceof FfmpegLoadError);

      return { error, bootstrap, report: error.candidates.at(-1)! };
    }

    throw new Error('expected discovery to fail');
  }

  test('rejects a library whose own version reports a different ABI major', () => {
    const { report, bootstrap } = failureFor({ majors: { avcodec: 62 } });

    assert.equal(report.reason, 'abi-mismatch');
    assert.match(report.detail ?? '', /reports libavcodec ABI 62, but 63 is required/);
    assert.equal(bootstrap.rollbacks, 1, 'already-loaded libraries are released');
    assert.equal(bootstrap.commits, 0);
  });

  test('rejects missing libraries without loading anything', () => {
    const { report, bootstrap } = failureFor({}, () =>
      installLibraries(managedLibrary(), process.platform, 'x', ['avfilter']),
    );

    assert.equal(report.reason, 'missing-libraries');
    assert.match(report.detail ?? '', /avfilter/);
    assert.deepEqual(bootstrap.loaded, []);
  });

  test('rejects libraries built for another CPU architecture before loading them', () => {
    const wrong = Buffer.alloc(64);

    wrong.writeUInt32BE(0x7f454c46, 0);
    wrong[5] = 1;
    wrong.writeUInt16LE(process.arch === 'arm64' ? 62 : 183, 18);

    const { report, bootstrap } = failureFor({}, () =>
      installLibraries(managedLibrary(), process.platform, wrong),
    );

    assert.equal(report.reason, 'architecture-mismatch');
    assert.deepEqual(bootstrap.loaded, []);
  });

  test('classifies unresolved transitive dependencies from the loader message', () => {
    const { report } = failureFor({
      failing: {
        match: 'avcodec',
        message: 'libx265.so.215: cannot open shared object file: No such file or directory',
      },
    });

    assert.equal(report.reason, 'unresolved-dependency');
    assert.match(report.detail ?? '', /libx265/);
  });

  test('classifies Windows error codes 126 (dependency) and 193 (architecture)', () => {
    const dependency = failureFor({
      failing: { match: 'avcodec', message: 'The specified module could not be found.', code: 126 },
    });

    assert.equal(dependency.report.reason, 'unresolved-dependency');

    const architecture = failureFor({
      failing: { match: 'avcodec', message: 'not a valid Win32 application', code: 193 },
    });

    assert.equal(architecture.report.reason, 'architecture-mismatch');
  });

  test('releases a rejected candidate before trying the next one', () => {
    const system = join(root, 'system');

    installLibraries(system);
    installLibraries(managedLibrary());

    const bootstrap = fakeBootstrap({ failing: { match: 'system', message: 'boom' } });
    const runtime = selectFfmpeg(
      bootstrap,
      environment({ systemDirectories: [system], homeDirectory: home() }),
    );

    assert.equal(runtime.source, 'managed');
    assert.deepEqual(
      runtime.considered.map((candidate) => `${candidate.source}:${candidate.outcome}`),
      ['system:rejected', 'managed:selected'],
    );
    assert.ok(bootstrap.loaded.every((path) => !path.includes('system')));
  });

  test('refuses libraries that resolve to different directories', () => {
    const other = join(root, 'elsewhere');
    const { report, bootstrap } = failureFor({ resolvedDirectory: { avcodec: other } });

    assert.equal(report.reason, 'mixed-installations');
    assert.match(report.detail ?? '', /different directories/);
    assert.equal(bootstrap.rollbacks, 1);
  });

  test('records library versions reported by the libraries themselves', () => {
    installLibraries(managedLibrary());

    const runtime = selectFfmpeg(fakeBootstrap(), environment({ homeDirectory: home() }));

    assert.ok(runtime.libraries.every((library) => library.version === `${library.major}.1.2`));
  });
});
