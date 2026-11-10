import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { ffmpegAbi } from './ffmpeg-abi.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const installSh = join(root, 'scripts/install.sh');
const installPs1 = join(root, 'scripts/install.ps1');
const windows = process.platform === 'win32';
const unixInstallerPlatform = process.platform === 'linux' || process.platform === 'darwin';

/** Library files the real FFmpeg 9 conda package provides on this platform. */
function libraryFiles() {
  return [...ffmpegAbi].map(([library, major]) => {
    const name = library.slice(3);

    if (windows) return `Library/bin/${name}-${major}.dll`;
    if (process.platform === 'darwin') return `lib/${library}.${major}.dylib`;

    return `lib/${library}.so.${major}`;
  });
}

/** Fresh sandbox: a HOME, an install dir whose path contains spaces, and a bin dir for fakes. */
function sandbox() {
  const base = mkdtempSync(join(tmpdir(), 'remedia installer '));
  const home = join(base, 'home dir');
  const installDir = join(base, 'install dir', '.remedia');
  const fakeBin = join(base, 'fake bin');
  const log = join(base, 'calls.log');

  mkdirSync(home, { recursive: true });
  mkdirSync(fakeBin, { recursive: true });
  writeFileSync(log, '');

  return {
    base,
    home,
    installDir,
    fakeBin,
    log,
    cleanup: () => rmSync(base, { recursive: true, force: true }),
  };
}

/** A fake micromamba that records its arguments and creates the FFmpeg 9 library files. */
function installFakeMicromamba(box, { omit = [] } = {}) {
  const files = libraryFiles().filter((file) => !omit.some((name) => file.includes(name)));

  if (windows) {
    const creates = files
      .map((file) => `type nul > "%PREFIX%\\${file.replaceAll('/', '\\')}"`)
      .join('\r\n');
    const dirs = [...new Set(files.map((file) => dirname(file).replaceAll('/', '\\')))]
      .map((dir) => `mkdir "%PREFIX%\\${dir}" 2>nul`)
      .join('\r\n');

    writeFileSync(
      join(box.fakeBin, 'micromamba.cmd'),
      [
        '@echo off',
        `echo micromamba %* >> "${box.log}"`,
        // Arguments are: <create|install> --yes --prefix <prefix> ...
        'set "PREFIX=%~4"',
        'mkdir "%PREFIX%\\conda-meta" 2>nul',
        dirs,
        creates,
        'exit /b 0',
        '',
      ].join('\r\n'),
    );

    return;
  }

  const script = [
    '#!/bin/sh',
    `printf 'micromamba %s\\n' "$*" >> '${box.log}'`,
    'prefix=""',
    'while [ $# -gt 0 ]; do [ "$1" = --prefix ] && prefix="$2"; shift; done',
    'mkdir -p "$prefix/conda-meta"',
    ...files.flatMap((file) => [`mkdir -p "$prefix/${dirname(file)}"`, `: > "$prefix/${file}"`]),
    '',
  ].join('\n');

  writeFileSync(join(box.fakeBin, 'micromamba'), script);
  chmodSync(join(box.fakeBin, 'micromamba'), 0o755);
}

/** A fake curl that records every invocation and always fails, so network use is detectable. */
function installFailingCurl(box) {
  const path = join(box.fakeBin, 'curl');

  writeFileSync(path, `#!/bin/sh\nprintf 'curl %s\\n' "$*" >> '${box.log}'\nexit 22\n`);
  chmodSync(path, 0o755);
}

function runSh(box, args, extraEnv = {}) {
  return spawnSync('sh', [installSh, ...args], {
    encoding: 'utf8',
    env: {
      PATH: `${box.fakeBin}:/usr/bin:/bin:/usr/sbin:/sbin`,
      HOME: box.home,
      SHELL: '/bin/sh',
      REMEDIA_INSTALL_DIR: box.installDir,
      ...extraEnv,
    },
  });
}

function runPs1(box, args) {
  return spawnSync('pwsh', ['-NoProfile', '-NonInteractive', '-File', installPs1, ...args], {
    encoding: 'utf8',
    env: {
      ...process.env,
      PATH: `${box.fakeBin};${process.env.PATH}`,
      REMEDIA_INSTALL_DIR: box.installDir,
    },
  });
}

function calls(box) {
  return readFileSync(box.log, 'utf8').split('\n').filter(Boolean);
}

function prefixLibraries(box) {
  return libraryFiles().map((file) => join(box.installDir, 'ffmpeg9', file));
}

test(
  'install.sh rejects --ffmpeg-only together with --no-ffmpeg',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();

    try {
      const result = runSh(box, ['--ffmpeg-only', '--no-ffmpeg']);

      assert.equal(result.status, 2);
      assert.match(result.stderr, /Conflicting options: --ffmpeg-only .* --no-ffmpeg/);
      assert.equal(existsSync(box.installDir), false);
    } finally {
      box.cleanup();
    }
  },
);

test('install.sh --help documents the FFmpeg-only mode', { skip: !unixInstallerPlatform }, () => {
  const box = sandbox();

  try {
    const result = runSh(box, ['--help']);

    assert.equal(result.status, 0);
    assert.match(result.stdout, /--ffmpeg-only/);
    assert.match(result.stdout, /--no-ffmpeg/);
  } finally {
    box.cleanup();
  }
});

test(
  'install.sh --ffmpeg-only installs FFmpeg without GitHub, server files, wrappers or PATH changes',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();

    try {
      installFakeMicromamba(box);
      installFailingCurl(box);

      const result = runSh(box, ['--ffmpeg-only']);

      assert.equal(result.status, 0, result.stderr);
      assert.deepEqual(
        calls(box).filter((line) => line.startsWith('curl')),
        [],
      );
      assert.deepEqual(readdirSync(box.installDir), ['ffmpeg9']);

      for (const library of prefixLibraries(box)) assert.ok(existsSync(library), library);

      assert.match(result.stdout, /Verified FFmpeg 9 libraries/);
      assert.ok(result.stdout.includes(join(box.installDir, 'ffmpeg9')));
      assert.deepEqual(readdirSync(box.home), [], 'shell profiles must not be modified');
      assert.match(calls(box)[0], /^micromamba create --yes --prefix .*ffmpeg9 .*ffmpeg=9\.0\.2$/);
    } finally {
      box.cleanup();
    }
  },
);

test(
  'install.sh --ffmpeg-only is repeatable and leaves an existing server install intact',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();

    try {
      installFakeMicromamba(box);
      installFailingCurl(box);

      const serverFiles = {
        'bin/remedia': '#!/bin/sh\necho wrapper\n',
        'bin/remedia-server': '#!/bin/sh\necho wrapper2\n',
        'libexec/remedia-server': 'server-binary',
        'libexec/remedia-server-runtime': 'runtime-binary',
        'licenses/LICENSE': 'license',
      };

      for (const [file, contents] of Object.entries(serverFiles)) {
        mkdirSync(dirname(join(box.installDir, file)), { recursive: true });
        writeFileSync(join(box.installDir, file), contents);
      }

      for (let run = 0; run < 2; run += 1) {
        const result = runSh(box, ['--ffmpeg-only']);

        assert.equal(result.status, 0, result.stderr);
      }

      const micromambaCalls = calls(box).filter((line) => line.startsWith('micromamba'));

      assert.match(micromambaCalls[0], /^micromamba create /);
      assert.match(micromambaCalls[1], /^micromamba install /);

      for (const [file, contents] of Object.entries(serverFiles)) {
        assert.equal(readFileSync(join(box.installDir, file), 'utf8'), contents, file);
      }

      assert.deepEqual(readdirSync(box.installDir).sort(), [
        'bin',
        'ffmpeg9',
        'libexec',
        'licenses',
      ]);
    } finally {
      box.cleanup();
    }
  },
);

test(
  'install.sh --ffmpeg-only fails clearly when an FFmpeg ABI library is missing',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();

    try {
      installFakeMicromamba(box, { omit: ['avfilter'] });
      installFailingCurl(box);

      const result = runSh(box, ['--ffmpeg-only']);

      assert.equal(result.status, 1);
      assert.match(result.stderr, /FFmpeg 9 runtime missing: .*avfilter/);
    } finally {
      box.cleanup();
    }
  },
);

test(
  'install.sh --ffmpeg-only downloads only micromamba when it is not installed',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();

    try {
      // curl writes a tarball containing a fake micromamba to the requested output file.
      const payload = join(box.base, 'payload');

      mkdirSync(join(payload, 'bin'), { recursive: true });
      installFakeMicromamba({ ...box, fakeBin: join(payload, 'bin') });
      rmSync(join(payload, 'bin/curl'), { force: true });

      const archive = join(box.base, 'micromamba.tar.bz2');
      const packed = spawnSync('tar', ['-cjf', archive, '-C', payload, 'bin/micromamba']);

      assert.equal(packed.status, 0, packed.stderr?.toString());

      const curl = join(box.fakeBin, 'curl');

      writeFileSync(
        curl,
        [
          '#!/bin/sh',
          `printf 'curl %s\\n' "$*" >> '${box.log}'`,
          'out=""',
          'while [ $# -gt 0 ]; do [ "$1" = -o ] && out="$2"; shift; done',
          `cp '${archive}' "$out"`,
          '',
        ].join('\n'),
      );
      chmodSync(curl, 0o755);

      const result = runSh(box, ['--ffmpeg-only']);

      assert.equal(result.status, 0, result.stderr);

      const curlCalls = calls(box).filter((line) => line.startsWith('curl'));

      assert.equal(curlCalls.length, 1);
      assert.match(curlCalls[0], /micro\.mamba\.pm/);
      assert.doesNotMatch(curlCalls.join('\n'), /github/);
      assert.deepEqual(readdirSync(box.installDir), ['ffmpeg9'], 'micromamba must not create bin/');
    } finally {
      box.cleanup();
    }
  },
);

test(
  'install.sh default mode still starts with the GitHub release lookup',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();

    try {
      installFakeMicromamba(box);
      installFailingCurl(box);

      const result = runSh(box, []);

      assert.notEqual(result.status, 0);
      assert.match(calls(box)[0], /api\.github\.com\/repos\/twlite\/remedia\/releases\/latest/);
      assert.equal(
        calls(box).some((line) => line.startsWith('micromamba')),
        false,
      );
    } finally {
      box.cleanup();
    }
  },
);

test('install.ps1 rejects -FFmpegOnly together with -NoFFmpeg', { skip: !windows }, () => {
  const box = sandbox();

  try {
    const result = runPs1(box, ['-FFmpegOnly', '-NoFFmpeg']);

    assert.notEqual(result.status, 0);
    assert.match(`${result.stdout}${result.stderr}`, /Conflicting options: -FFmpegOnly/);
  } finally {
    box.cleanup();
  }
});

test(
  'install.ps1 -FFmpegOnly installs FFmpeg only, repeatably, in a path containing spaces',
  { skip: !windows },
  () => {
    const box = sandbox();

    try {
      installFakeMicromamba(box);

      const serverMarker = join(box.installDir, 'libexec', 'current.txt');

      mkdirSync(dirname(serverMarker), { recursive: true });
      writeFileSync(serverMarker, 'v0.0.1');

      for (let run = 0; run < 2; run += 1) {
        const result = runPs1(box, ['-FFmpegOnly']);

        assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
        assert.match(result.stdout, /Verified FFmpeg 9 libraries/);
        assert.doesNotMatch(result.stdout, /Checking GitHub releases/);
      }

      for (const library of prefixLibraries(box)) assert.ok(existsSync(library), library);

      assert.equal(readFileSync(serverMarker, 'utf8'), 'v0.0.1');
      assert.equal(existsSync(join(box.installDir, 'bin')), false, 'no bin directory');

      const micromambaCalls = calls(box).filter((line) => line.startsWith('micromamba'));

      assert.match(micromambaCalls[0], /^micromamba create /);
      assert.match(micromambaCalls[1], /^micromamba install /);
    } finally {
      box.cleanup();
    }
  },
);

test(
  'install.ps1 can be invoked through the documented scriptblock form',
  { skip: !windows },
  () => {
    const box = sandbox();

    try {
      installFakeMicromamba(box);

      const command = `& ([scriptblock]::Create((Get-Content -Raw -LiteralPath '${installPs1}'))) -FFmpegOnly`;
      const result = spawnSync('pwsh', ['-NoProfile', '-NonInteractive', '-Command', command], {
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: `${box.fakeBin};${process.env.PATH}`,
          REMEDIA_INSTALL_DIR: box.installDir,
        },
      });

      assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
      assert.match(result.stdout, /Verified FFmpeg 9 libraries/);
    } finally {
      box.cleanup();
    }
  },
);

test(
  'install.ps1 rejects conflicting options through the scriptblock form on every platform',
  { skip: process.platform === 'win32' },
  () => {
    // The Windows-only platform guard comes after the conflict check, so this runs everywhere
    // pwsh exists; skip quietly where PowerShell is unavailable.
    const probe = spawnSync('pwsh', ['-NoProfile', '-Command', '$PSVersionTable.PSVersion.Major'], {
      encoding: 'utf8',
    });

    if (probe.error || probe.status !== 0) return;

    const command = `& ([scriptblock]::Create((Get-Content -Raw -LiteralPath '${installPs1}'))) -FFmpegOnly -NoFFmpeg`;
    const result = spawnSync('pwsh', ['-NoProfile', '-NonInteractive', '-Command', command], {
      encoding: 'utf8',
    });

    assert.notEqual(result.status, 0);
    assert.match(`${result.stdout}${result.stderr}`, /Conflicting options: -FFmpegOnly/);
  },
);
