import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  existsSync,
  writeFileSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
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

for (const [license, expected] of [
  ['MIT', 0],
  ['LicenseRef-nonfree', 1],
]) {
  test(`license inventory handles a clean checkout with ${license}`, () => {
    const directory = mkdtempSync(join(tmpdir(), 'remedia-license-audit-'));
    try {
      mkdirSync(join(directory, 'src'));
      writeFileSync(join(directory, 'src/lib.rs'), '');
      writeFileSync(
        join(directory, 'Cargo.toml'),
        `[package]\nname = "audit-fixture"\nversion = "0.1.0"\nlicense = "${license}"\nedition = "2021"\n`,
      );
      const lock = spawnSync('cargo', ['generate-lockfile', '--offline'], {
        cwd: directory,
        encoding: 'utf8',
      });
      assert.equal(lock.status, 0, lock.stderr);
      const script = resolve('.github/scripts/audit-linked-licenses.mjs');
      const result = spawnSync(process.execPath, [script], {
        cwd: directory,
        encoding: 'utf8',
        env: { ...process.env, CARGO_NET_OFFLINE: 'true' },
      });
      assert.equal(result.status, expected, result.stderr);
      const output = join(directory, 'target/linked-license-inventory.json');
      if (expected === 0) {
        const inventory = JSON.parse(readFileSync(output, 'utf8'));
        assert.equal(inventory[0].name, 'audit-fixture');
        assert.equal(inventory[0].license, license);
      } else {
        assert.match(result.stderr, /Unaudited license LicenseRef-nonfree/);
        assert.equal(existsSync(output), false);
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
}

test('Windows FFmpeg port patch writes literal files without provider-specific parameters', (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const directory = mkdtempSync(join(tmpdir(), 'remedia port patch '));
  try {
    const workflow = readFileSync('.github/scripts/build-ffmpeg-windows.ps1', 'utf8');
    const snippets = [
      ...workflow.matchAll(/\$portFile =[^]*?\[IO\.File\]::WriteAllText[^\n]*/g),
    ].map(([script]) => script);
    assert.equal(snippets.length, 1, 'shared Windows recipe must be exercised');
    mkdirSync(join(directory, 'ports/ffmpeg'), { recursive: true });
    mkdirSync(join(directory, '.github/native-deps'), { recursive: true });
    const portFile = join(directory, 'ports/ffmpeg/portfile.cmake');
    const original = 'set(OPTIONS "--enable-pic --disable-doc")\n';
    writeFileSync(
      join(directory, '.github/native-deps/ffmpeg-audio-options.txt'),
      '--disable-decoders\n--enable-decoder=aac,mp3\n',
    );
    for (const snippet of snippets) {
      writeFileSync(portFile, original);
      const script = join(directory, 'patch.ps1');
      writeFileSync(
        script,
        '$ErrorActionPreference = "Stop"\n$vcpkgRoot = $env:GITHUB_WORKSPACE\n' + snippet,
      );
      const result = spawnSync('pwsh', ['-NoProfile', '-File', script], {
        encoding: 'utf8',
        env: { ...process.env, GITHUB_WORKSPACE: directory },
      });
      assert.equal(result.status, 0, result.stderr);
      assert.equal(
        readFileSync(portFile, 'utf8'),
        'set(OPTIONS "--disable-decoders --enable-decoder=aac,mp3 --enable-pic --disable-doc")\n',
      );
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

for (const [compiler, hostCompiler] of [
  ['', ''],
  ['target-clang', 'host-clang'],
]) {
  test(`native FFmpeg recipe selects ${compiler || 'cc'} explicitly`, (t) => {
    if (process.platform === 'win32') return t.skip('POSIX recipe');
    const directory = mkdtempSync(join(tmpdir(), 'remedia native recipe '));
    try {
      const bin = join(directory, 'bin');
      const source = join(directory, 'remedia-static/ffmpeg');
      const log = join(directory, 'configure-arguments');
      mkdirSync(bin);
      mkdirSync(join(source, '.git'), { recursive: true });
      mkdirSync(join(source, 'ffbuild'));
      for (const [name, content] of [
        ['uname', '#!/bin/sh\nprintf Darwin\n'],
        [
          'git',
          '#!/bin/sh\ncase "$*" in *ls-files*) exec /usr/bin/git "$@" ;; *rev-parse*) printf 946fcce07b6dcd0331c8cc609192aeff5e1924f8 ;; *show*) printf 1790000000 ;; esac\nexit 0\n',
        ],
        ['make', '#!/bin/sh\nexit 0\n'],
      ]) {
        const path = join(bin, name);
        writeFileSync(path, content);
        chmodSync(path, 0o755);
      }
      const configure = join(source, 'configure');
      writeFileSync(
        configure,
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$REMEDIA_CONFIGURE_LOG"\nprintf "%s\\n" "$@" > ffbuild/config.mak\n: > config.h\n',
      );
      chmodSync(configure, 0o755);
      const result = spawnSync('bash', ['.github/scripts/build-ffmpeg-static.sh'], {
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: `${bin}:${process.env.PATH}`,
          RUNNER_TEMP: directory,
          FFMPEG_DIR: join(directory, 'prefix'),
          GITHUB_ENV: join(directory, 'environment'),
          REMEDIA_CONFIGURE_LOG: log,
          CC: compiler,
          HOST_CC: hostCompiler,
          MAKE: 'make',
        },
      });
      assert.equal(result.status, 0, result.stderr);
      const configureArguments = readFileSync(log, 'utf8').split('\n');
      assert.ok(configureArguments.includes(`--cc=${compiler || 'cc'}`));
      assert.ok(configureArguments.includes(`--host-cc=${hostCompiler || 'cc'}`));

      // Static OpenSSL's pthread dependency is private pkg-config metadata. Resolve
      // an actual .pc fixture using the flags passed by the recipe, rather than
      // relying on pthread symbols being merged into libc on the test host.
      const packageDirectory = join(directory, 'pkgconfig');
      mkdirSync(packageDirectory);
      writeFileSync(
        join(packageDirectory, 'openssl.pc'),
        'Name: OpenSSL fixture\nDescription: Static transitive dependency regression\nVersion: 3.6.0\nLibs: -lssl -lcrypto\nLibs.private: -pthread\n',
      );
      const flags = configureArguments
        .filter((argument) => argument.startsWith('--pkg-config-flags='))
        .flatMap((argument) => argument.slice('--pkg-config-flags='.length).split(' '));
      const dependencies = spawnSync('pkg-config', ['--libs', ...flags, 'openssl'], {
        encoding: 'utf8',
        env: {
          ...process.env,
          PKG_CONFIG_PATH: packageDirectory,
          PKG_CONFIG_LIBDIR: packageDirectory,
        },
      });
      assert.equal(dependencies.status, 0, dependencies.stderr);
      assert.ok(
        dependencies.stdout.split(/\s+/).includes('-pthread'),
        'the OpenSSL link probe must receive private pthread linkage',
      );
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
}

test('Windows jobs select the configured MSVC host and target linker directories', (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const directory = mkdtempSync(join(tmpdir(), 'remedia msvc linker '));
  try {
    const workflow = readFileSync('.github/scripts/build-ffmpeg-windows.ps1', 'utf8');
    const snippets = [...workflow.matchAll(/\$msvcTarget =[^]*?"CARGO_TARGET_[^\n]+/g)].map(
      ([script]) => script,
    );
    assert.equal(snippets.length, 1);
    for (const [architecture, host, target, triple, triplet] of [
      ['AMD64', 'x64', 'x64', 'x86_64-pc-windows-msvc', 'x64-windows-static-md'],
      ['AMD64', 'x64', 'x86', 'i686-pc-windows-msvc', 'x86-windows-static-md'],
      ['ARM64', 'arm64', 'arm64', 'aarch64-pc-windows-msvc', 'arm64-windows-static-md'],
    ]) {
      const linkerDirectory = join(directory, 'bin', `Host${host}`, target);
      mkdirSync(linkerDirectory, { recursive: true });
      const linker = join(linkerDirectory, 'link.exe');
      writeFileSync(linker, 'fixture');
      for (const snippet of snippets) {
        const script = join(directory, 'linker.ps1');
        const environment = join(directory, 'environment');
        writeFileSync(environment, '');
        writeFileSync(
          script,
          `$ErrorActionPreference = 'Stop'\n$triplet = '${triplet}'\n${snippet}`,
        );
        const result = spawnSync('pwsh', ['-NoProfile', '-File', script], {
          encoding: 'utf8',
          env: {
            ...process.env,
            VCToolsInstallDir: directory,
            VSCMD_ARG_HOST_ARCH: host,
            PROCESSOR_ARCHITECTURE: architecture,
            RUST_TARGET: triple,
            GITHUB_ENV: environment,
          },
        });
        assert.equal(result.status, 0, result.stderr);
        assert.ok(readFileSync(environment, 'utf8').includes(`=${linker}`));
      }
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('cross recipes leave musl Rust linking to cargo-zigbuild and retain target C tools', (t) => {
  if (process.platform === 'win32') return t.skip('POSIX recipe');
  const directory = mkdtempSync(join(tmpdir(), 'remedia musl linker '));
  try {
    const recipe = readFileSync('.github/scripts/build-ffmpeg-cross.sh', 'utf8');
    const block = recipe.slice(recipe.indexOf('target_env='), recipe.indexOf('configure_args=('));
    assert.ok(block.includes('GITHUB_ENV'));
    for (const [target, zigTriple] of [
      ['x86_64-unknown-linux-musl', 'x86_64-linux-musl'],
      ['aarch64-unknown-linux-musl', 'aarch64-linux-musl'],
      ['armv7-unknown-linux-gnueabihf', ''],
      ['aarch64-linux-android', ''],
      ['armv7-linux-androideabi', ''],
    ]) {
      const output = join(directory, target);
      const result = spawnSync('bash', ['-c', 'set -euo pipefail\n' + block], {
        encoding: 'utf8',
        env: {
          ...process.env,
          target,
          zig_triple: zigTriple,
          local_cc: '/target tools/cc',
          local_ar: '/target tools/ar',
          GITHUB_ENV: output,
        },
      });
      assert.equal(result.status, 0, result.stderr);
      const environment = Object.fromEntries(
        readFileSync(output, 'utf8')
          .trim()
          .split('\n')
          .map((line) => {
            const equals = line.indexOf('=');
            return [line.slice(0, equals), line.slice(equals + 1)];
          }),
      );
      const normalized = target.replaceAll('-', '_');
      for (const variant of [target, normalized, normalized.toUpperCase()]) {
        assert.equal(environment[`CC_${variant}`], '/target tools/cc');
        assert.equal(environment[`AR_${variant}`], '/target tools/ar');
      }
      const linker = `CARGO_TARGET_${normalized.toUpperCase()}_LINKER`;
      assert.equal(environment[linker], zigTriple ? undefined : '/target tools/cc');
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('Windows jobs expose vcpkg metadata at the location used by the pinned Rust helper', (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const directory = mkdtempSync(join(tmpdir(), 'remedia vcpkg metadata '));
  try {
    const workflow = readFileSync('.github/scripts/build-ffmpeg-windows.ps1', 'utf8');
    const snippets = [...workflow.matchAll(/\$triplet =[^]*?(?=# Git for Windows)/g)].map(
      ([script]) => script,
    );
    assert.equal(snippets.length, 1);
    for (const [target, triplet] of [
      ['i686-pc-windows-msvc', 'x86-windows-static-md'],
      ['x86_64-pc-windows-msvc', 'x64-windows-static-md'],
      ['aarch64-pc-windows-msvc', 'arm64-windows-static-md'],
    ]) {
      for (const snippet of snippets) {
        const script = join(directory, 'metadata.ps1');
        const output = join(directory, 'environment');
        const root = join(directory, 'remedia-vcpkg');
        writeFileSync(output, '');
        // Execute path selection and environment exports, without downloading or
        // building FFmpeg; both sections surround the existing vcpkg invocation.
        const configuration = snippet.slice(0, snippet.indexOf('Push-Location'));
        const exports = snippet.slice(snippet.indexOf('"VCPKG_ROOT='));
        writeFileSync(
          script,
          '$ErrorActionPreference = "Stop"\n$vcpkgRoot = $env:TEST_VCPKG_ROOT\n' +
            configuration +
            exports,
        );
        const result = spawnSync('pwsh', ['-NoProfile', '-File', script], {
          encoding: 'utf8',
          env: {
            ...process.env,
            TEST_VCPKG_ROOT: root,
            RUNNER_TEMP: directory,
            GITHUB_WORKSPACE: directory,
            RUST_TARGET: target,
            GITHUB_ENV: output,
          },
        });
        assert.equal(result.status, 0, result.stderr);
        const environment = Object.fromEntries(
          readFileSync(output, 'utf8')
            .trim()
            .split('\n')
            .map((line) => {
              const equals = line.indexOf('=');
              return [line.slice(0, equals), line.slice(equals + 1)];
            }),
        );
        assert.equal(environment.VCPKG_ROOT, root);
        assert.equal(environment.VCPKG_INSTALL_ROOT, join(root, 'installed'));
        assert.equal(environment.VCPKG_TARGET_TRIPLET, triplet);
        assert.equal(environment.VCPKGRS_TRIPLET, triplet);
      }
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('core build metadata supplies Schannel libraries to every MSVC target', () => {
  const directory = mkdtempSync(join(tmpdir(), 'remedia schannel linkage '));
  try {
    const binary = join(directory, `core-build${process.platform === 'win32' ? '.exe' : ''}`);
    const compile = spawnSync(
      'rustc',
      ['--crate-name', 'remedia_core_build_probe', 'crates/remedia-core/build.rs', '-o', binary],
      { encoding: 'utf8' },
    );
    assert.equal(compile.status, 0, compile.stderr);
    for (const [target, os, environment] of [
      ['i686-pc-windows-msvc', 'windows', 'msvc'],
      ['x86_64-pc-windows-msvc', 'windows', 'msvc'],
      ['aarch64-pc-windows-msvc', 'windows', 'msvc'],
      ['x86_64-pc-windows-gnu', 'windows', 'gnu'],
      ['x86_64-unknown-linux-musl', 'linux', 'musl'],
      ['aarch64-apple-darwin', 'macos', ''],
    ]) {
      const result = spawnSync(binary, [], {
        encoding: 'utf8',
        env: {
          ...process.env,
          TARGET: target,
          CARGO_CFG_TARGET_OS: os,
          CARGO_CFG_TARGET_ENV: environment,
        },
      });
      assert.equal(result.status, 0, result.stderr);
      const libraries = result.stdout
        .split(/\r?\n/)
        .filter((line) => line.startsWith('cargo:rustc-link-lib='));
      assert.deepEqual(
        libraries,
        environment === 'msvc'
          ? ['cargo:rustc-link-lib=ncrypt', 'cargo:rustc-link-lib=crypt32']
          : [],
        target,
      );
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('source capture contains only configuration records, never source or compiled caches', (t) => {
  if (process.platform === 'win32') return t.skip('POSIX shell fixture');
  const directory = mkdtempSync(join(tmpdir(), 'remedia source capture '));
  try {
    const prefix = join(directory, 'installed');
    const records = join(prefix, 'share/remedia-build');
    mkdirSync(records, { recursive: true });
    writeFileSync(join(records, 'config.h'), '#define CONFIG_GPL 1\n');
    writeFileSync(join(records, 'config.mak'), 'configured audio build');
    mkdirSync(join(prefix, 'lib'));
    writeFileSync(join(prefix, 'lib/avcodec.lib'), 'must not be captured');
    const result = spawnSync('bash', ['.github/scripts/capture-native-source.sh'], {
      encoding: 'utf8',
      env: {
        ...process.env,
        RUNNER_TEMP: directory,
        RUST_TARGET: 'i686-pc-windows-msvc',
        REMEDIA_STATIC_FFMPEG_DIR: prefix,
      },
    });
    assert.equal(result.status, 0, result.stderr);
    const archive = join(directory, 'native-source-i686-pc-windows-msvc.tar.gz');
    const entries = spawnSync('tar', ['-tzf', archive], { encoding: 'utf8' });
    assert.equal(entries.status, 0, entries.stderr);
    for (const file of ['config.h', 'config.mak', 'compiler.txt', 'rustc.txt'])
      assert.ok(entries.stdout.includes('./' + file));
    assert.doesNotMatch(entries.stdout, /\.lib|buildtrees|downloads|vendor/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('source archives are byte-identical after staging timestamps change', (t) => {
  if (process.platform === 'win32') return t.skip('POSIX Python fixture');
  const directory = mkdtempSync(join(tmpdir(), 'remedia deterministic source '));
  try {
    const source = join(directory, 'source');
    mkdirSync(source);
    const file = join(source, 'configuration');
    writeFileSync(file, 'exact source');
    const first = join(directory, 'first.tar.gz');
    const second = join(directory, 'second.tar.gz');
    const script = '.github/scripts/source-archive.py';
    assert.equal(spawnSync('python3', [script, source, first]).status, 0);
    writeFileSync(file, 'exact source');
    assert.equal(spawnSync('python3', [script, source, second]).status, 0);
    assert.deepEqual(readFileSync(first), readFileSync(second));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
