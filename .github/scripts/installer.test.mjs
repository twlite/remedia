import { createHash } from 'node:crypto';

import assert from 'node:assert/strict';
import { spawn as spawnProcess, spawnSync as spawnProcessSync } from 'node:child_process';
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
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const installSh = join(root, 'scripts/install.sh');
const installPs1 = join(root, 'scripts/install.ps1');
const windows = process.platform === 'win32';
const unixInstallerPlatform = process.platform === 'linux' || process.platform === 'darwin';

// These tests need child output, not console windows. Hiding the window also avoids
// PowerShell's taskbar jump-list heap corruption on Windows ARM64:
// https://github.com/dotnet/runtime/issues/131636
function spawn(command, args, options = {}) {
  return spawnProcess(command, args, { ...options, windowsHide: true });
}

function spawnSync(command, args, options = {}) {
  return spawnProcessSync(command, args, { ...options, windowsHide: true });
}

// Match the Windows installer: avoid Git's GNU tar interpreting C:\... as a remote archive.
const windowsTar = process.env.SystemRoot && join(process.env.SystemRoot, 'System32', 'tar.exe');
const archiveTar = windows && windowsTar && existsSync(windowsTar) ? windowsTar : 'tar';

/** Build fixture archives with platform-native tar and retain diagnostics on failure. */
function packArchive(archive, payload, entries, compression = 'z') {
  const result = spawnSync(archiveTar, [`-c${compression}f`, archive, '-C', payload, ...entries], {
    encoding: 'utf8',
  });

  assert.equal(result.status, 0, `${archiveTar}: ${result.error ?? ''}\n${result.stderr}`);
}

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

function runSh(box, args, extraEnv = {}) {
  return spawnSync('sh', [installSh, ...args], {
    encoding: 'utf8',
    env: {
      PATH: `${box.fakeBin}:/usr/bin:/bin:/usr/sbin:/sbin`,
      HOME: box.home,
      SHELL: '/bin/sh',
      REMEDIA_INSTALL_DIR: box.installDir,
      REMEDIA_NO_MODIFY_PATH: '1',
      ...extraEnv,
    },
  });
}

function calls(box) {
  return readFileSync(box.log, 'utf8').split('\n').filter(Boolean);
}

/** A generated release archive and fake GitHub responses exercise real installer file operations. */
function fakeRelease(box, version, { failDownload = false, badChecksum = false } = {}) {
  const target =
    process.platform === 'darwin'
      ? `${process.arch === 'arm64' ? 'aarch64' : 'x86_64'}-apple-darwin`
      : `${process.arch === 'arm64' ? 'aarch64' : 'x86_64'}-unknown-linux-gnu`;
  const payload = join(box.base, 'release');
  mkdirSync(payload, { recursive: true });
  const binary = join(payload, `remedia-server-${target}`);
  writeFileSync(
    binary,
    `#!/bin/sh\nif [ "\${1:-}" = --pair ]; then printf '%s\\n' "$0"; exec "$(dirname "$0")/remedia-server-runtime"; fi\necho 'remedia-server ${version} (commit fixture)'\n`,
  );
  chmodSync(binary, 0o755);
  writeFileSync(
    join(payload, `remedia-server-runtime-${target}`),
    `#!/bin/sh\nprintf '%s\\n' 'runtime ${version}'\n`,
  );
  for (const name of [
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'THIRD_PARTY_LICENSES.txt',
    'LICENSES.md',
    'COPYING.GPLv3',
  ])
    writeFileSync(join(payload, name), name);
  const archive = join(box.base, 'release.tar.gz');
  packArchive(archive, payload, readdirSync(payload));
  const digest = badChecksum
    ? '0'.repeat(64)
    : createHash('sha256').update(readFileSync(archive)).digest('hex');
  const metadata = join(box.base, 'release.json');
  writeFileSync(
    metadata,
    JSON.stringify(
      {
        tag_name: `v${version}`,
        assets: [{ name: `remedia-server-${target}.tar.gz`, digest: `sha256:${digest}` }],
      },
      null,
      2,
    ),
  );
  const curl = join(box.fakeBin, 'curl');
  writeFileSync(
    curl,
    `#!/bin/sh\nprintf '%s\\n' "$*" >> '${box.log}'\nout=''\nurl=''\nwhile [ $# -gt 0 ]; do\n case "$1" in -o) shift; out="$1" ;; https:*) url="$1" ;; esac\n shift\ndone\ncase "$url" in\n *api.github.com*) cp '${metadata}' "$out"; printf 200 ;;\n *) ${failDownload ? 'exit 22' : `cp '${archive}' "$out"`} ;;\nesac\n`,
  );
  chmodSync(curl, 0o755);
}

test('Unix PATH opt-out and ZDOTDIR handling', { skip: !unixInstallerPlatform }, () => {
  const box = sandbox();
  try {
    fakeRelease(box, '0.3.0');
    const zdotdir = join(box.base, 'zsh config');
    mkdirSync(zdotdir);
    const profile = join(zdotdir, '.zshrc');
    writeFileSync(profile, '# fixture\n');
    const env = { SHELL: '/bin/zsh', ZDOTDIR: zdotdir };
    for (const [args, extra] of [
      [[], env],
      [['--no-modify-path'], { ...env, REMEDIA_NO_MODIFY_PATH: '0' }],
    ]) {
      const result = runSh(box, args, extra);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(readFileSync(profile, 'utf8'), '# fixture\n');
      assert.equal(existsSync(join(box.home, '.zshrc')), false);
    }
    const result = runSh(box, [], { ...env, REMEDIA_NO_MODIFY_PATH: '0' });
    assert.equal(result.status, 0, result.stderr);
    const registered = readFileSync(profile, 'utf8');
    assert.ok(registered.includes(join(box.installDir, 'bin')));
    assert.equal(existsSync(join(box.home, '.zshrc')), false);
    assert.equal(runSh(box, [], { ...env, REMEDIA_NO_MODIFY_PATH: '0' }).status, 0);
    assert.equal(readFileSync(profile, 'utf8'), registered);
  } finally {
    box.cleanup();
  }
});

test(
  'Bash installs, skips identical full versions, updates between dev and stable, and retains the active pair on failure',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();
    try {
      for (const version of ['0.1.0-dev.7', '0.1.0-dev.8', '0.1.0', '0.2.0-dev.1']) {
        fakeRelease(box, version);
        let result = runSh(box, ['--no-ffmpeg']);
        assert.equal(result.status, 0, result.stderr);
        const pointer = readFileSync(join(box.installDir, 'libexec/current.txt'), 'utf8');
        assert.equal(pointer, 'v' + version);
        assert.equal(
          readdirSync(join(box.installDir, 'libexec')).some((name) => name.startsWith('.stage-')),
          false,
        );
        const command = join(box.installDir, 'bin/remedia');
        assert.match(
          spawnSync(command, ['--version'], { encoding: 'utf8' }).stdout,
          new RegExp(version.replaceAll('.', '\\.')),
        );
        writeFileSync(box.log, '');
        result = runSh(box, []);
        assert.equal(result.status, 0, result.stderr);
        assert.match(result.stdout, /already current/);
        assert.equal(calls(box).length, 1, 'only release metadata is requested');
        assert.equal(readFileSync(join(box.installDir, 'libexec/current.txt'), 'utf8'), pointer);
      }
      const pointer = readFileSync(join(box.installDir, 'libexec/current.txt'), 'utf8');
      for (const options of [{ failDownload: true }, { badChecksum: true }]) {
        fakeRelease(box, '0.3.0', options);
        const result = runSh(box, ['--no-ffmpeg']);
        assert.notEqual(result.status, 0);
        assert.equal(readFileSync(join(box.installDir, 'libexec/current.txt'), 'utf8'), pointer);
      }
      fakeRelease(box, '0.3.0');
      assert.equal(runSh(box, []).status, 0);
      assert.notEqual(readFileSync(join(box.installDir, 'libexec/current.txt'), 'utf8'), pointer);
    } finally {
      box.cleanup();
    }
  },
);

for (const shell of windows ? ['powershell.exe', 'pwsh'] : ['pwsh']) {
  test(`${shell} downloader supports direct downloads and release-asset API fallback`, async (t) => {
    if (spawnSync(shell, ['-NoProfile', '-Command', 'exit 0']).error)
      return t.skip(`${shell} unavailable`);
    const box = sandbox();
    const requests = [];
    const server = createServer((request, response) => {
      requests.push([request.url, request.headers.accept]);
      if (request.url === '/failed') {
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, { 'Content-Length': 7 });
      response.end('archive');
    });
    await new Promise((finish) => server.listen(0, '127.0.0.1', finish));
    try {
      const source = readFileSync(installPs1, 'utf8');
      const functions = source.slice(
        source.indexOf('function Write-InstallStatus'),
        source.indexOf('Write-InstallStatus "Checking GitHub releases'),
      );
      const start = source.indexOf(
        '      try {\n        Save-InstallDownload -Url $asset.browser_download_url',
      );
      assert.notEqual(start, -1, 'production download fallback is present');
      const end = source.indexOf(
        "\n    }\n    Write-InstallStatus 'Verifying server archive",
        start,
      );
      assert.notEqual(end, -1);
      const download = source.slice(start, end);
      const script = join(box.base, 'download.ps1');
      const base = `http://127.0.0.1:${server.address().port}`;
      // Execute the production fallback block, including the same HttpClient constructor.
      writeFileSync(
        script,
        `$ErrorActionPreference = 'Stop'\n${functions}\n$archive = '${join(box.base, 'download')}'\nSave-InstallDownload -Url '${base}/direct' -Destination $archive -Label direct -Attempts 1\n$asset = @{ browser_download_url = '${base}/failed'; url = '${base}/asset'; size = 7 }\n${download}\nif ((Get-Content -Raw $archive) -ne 'archive') { throw 'Bad download' }\n`,
      );
      const result = await new Promise((finish) => {
        const child = spawn(shell, ['-NoProfile', '-NonInteractive', '-File', script]);
        let output = '';
        child.stdout.on('data', (data) => {
          output += data;
        });
        child.stderr.on('data', (data) => {
          output += data;
        });
        child.on('close', (status) => finish({ status, output }));
      });
      assert.equal(result.status, 0, result.output);
      assert.deepEqual(
        requests.map(([url]) => url),
        ['/direct', '/failed', '/asset'],
      );
      assert.ok(requests.every(([, accept]) => accept === 'application/octet-stream'));
    } finally {
      server.closeAllConnections();
      await new Promise((finish) => server.close(finish));
      box.cleanup();
    }
  });
}

test('PowerShell installation skips current versions and keeps its pointer on download or checksum failure', async (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const box = sandbox();
  try {
    // Reproduce the runner's PATH shadowing. On Unix, emulate the native Windows tar
    // location with a launcher so the same selection and extraction are exercised locally.
    let systemRoot = process.env.SystemRoot;
    if (windows) {
      writeFileSync(join(box.fakeBin, 'tar.cmd'), '@echo off\r\nexit /b 99\r\n');
    } else {
      systemRoot = join(box.base, 'Windows');
      const nativeTar = join(systemRoot, 'System32', 'tar.exe');
      mkdirSync(dirname(nativeTar), { recursive: true });
      writeFileSync(nativeTar, '#!/bin/sh\nexec /usr/bin/tar "$@"\n');
      chmodSync(nativeTar, 0o755);
      const shadowTar = join(box.fakeBin, 'tar');
      writeFileSync(shadowTar, '#!/bin/sh\necho "wrong tar from PATH" >&2\nexit 99\n');
      chmodSync(shadowTar, 0o755);
    }

    const source = readFileSync(installPs1, 'utf8');
    const functions = source.slice(
      source.indexOf('function Write-InstallStatus'),
      source.indexOf('Write-InstallStatus "Checking GitHub releases'),
    );
    const workflow = source.slice(
      source.indexOf('Write-InstallStatus "Checking GitHub releases..."'),
    );
    const payload = join(box.base, 'payload');
    mkdirSync(payload);
    for (const file of [
      'remedia-server-x86_64-pc-windows-msvc.exe',
      'remedia-server-runtime-x86_64-pc-windows-msvc.exe',
      'LICENSE',
      'THIRD_PARTY_NOTICES.md',
      'THIRD_PARTY_LICENSES.txt',
      'LICENSES.md',
      'COPYING.GPLv3',
    ])
      writeFileSync(join(payload, file), file);
    const archive = join(box.base, 'archive.tar.gz');
    packArchive(archive, payload, readdirSync(payload));
    const script = join(box.base, 'workflow.ps1');
    const pointer = join(box.installDir, 'libexec/current.txt');
    for (const [version, failure] of [
      ['0.1.0-dev.7', ''],
      ['0.1.0-dev.7', 'skip'],
      ['0.1.0', ''],
      ['0.2.0', 'download'],
      ['0.2.0', 'checksum'],
    ]) {
      const digest = createHash('sha256').update(readFileSync(archive)).digest('hex');
      const previous = existsSync(pointer) ? readFileSync(pointer, 'utf8') : null;
      writeFileSync(
        script,
        `$ErrorActionPreference = 'Stop'\n$InstallDir = '${box.installDir}'\n$Version = 'latest'\n$repo = 'fixture'\n$NoFFmpeg = $true\n${functions}\nfunction Add-RemediaToPath {}\nfunction Invoke-RestMethod { return @{ tag_name = 'v${version}'; assets = @(@{ name = 'remedia-server-x86_64-pc-windows-msvc.tar.gz'; digest = 'sha256:${failure === 'checksum' ? '0'.repeat(64) : digest}'; browser_download_url = 'direct'; url = 'api'; size = 7 }) } }\nfunction Save-InstallDownload { param($Url, $Destination, $Label, $ExpectedBytes, $Attempts) ${failure === 'download' || failure === 'skip' ? "throw 'unexpected download'" : `Copy-Item -LiteralPath '${archive}' -Destination $Destination`} }\n${workflow}`,
      );
      const result = spawnSync('pwsh', ['-NoProfile', '-NonInteractive', '-File', script], {
        encoding: 'utf8',
        env: {
          ...process.env,
          SystemRoot: systemRoot,
          PATH: `${box.fakeBin}${windows ? ';' : ':'}${process.env.PATH}`,
        },
      });
      if (failure === 'download' || failure === 'checksum') {
        assert.notEqual(result.status, 0);
        assert.equal(readFileSync(pointer, 'utf8'), previous);
      } else {
        assert.equal(result.status, 0, result.stdout + result.stderr);
        if (failure === 'skip') assert.match(result.stdout, /already current/);
        else {
          assert.match(result.stdout, /Installed Remedia/);
          assert.notEqual(readFileSync(pointer, 'utf8'), previous);
          assert.equal(
            readFileSync(
              join(box.installDir, 'libexec', readFileSync(pointer, 'utf8'), 'version.txt'),
              'utf8',
            ),
            `v${version}`,
          );
        }
      }
    }
  } finally {
    box.cleanup();
  }
});

for (const shell of windows ? ['powershell.exe', 'pwsh'] : ['pwsh']) {
  test(`${shell} validates Windows x64 and Windows 11 ARM64 independently of shell architecture`, (t) => {
    if (spawnSync(shell, ['-NoProfile', '-Command', 'exit 0']).error)
      return t.skip(`${shell} unavailable`);

    const source = readFileSync(installPs1, 'utf8');
    const predicate = source.slice(
      source.indexOf('function Test-RemediaWindowsPlatform'),
      source.indexOf('$osArchitecture ='),
    );
    const architecture = source.slice(
      source.indexOf('$osArchitecture ='),
      source.indexOf('if (-not (Test-RemediaWindowsPlatform'),
    );
    const result = spawnSync(
      shell,
      [
        '-NoProfile',
        '-NonInteractive',
        '-Command',
        `${predicate}
      $ErrorActionPreference = 'Stop'
      foreach ($arch in @('AMD64', 'X64', 'ARM64')) {
        if (-not (Test-RemediaWindowsPlatform $arch 22000 $true)) { throw "Rejected $arch" }
      }
      if (Test-RemediaWindowsPlatform ARM64 19045 $true) { throw 'Accepted Windows 10 ARM64' }
      if (Test-RemediaWindowsPlatform x86 22631 $true) { throw 'Accepted x86' }
      if (Test-RemediaWindowsPlatform IA64 22631 $true) { throw 'Accepted IA64' }
      if (Test-RemediaWindowsPlatform AMD64 22631 $false) { throw 'Accepted non-Windows' }
      foreach ($native in @('AMD64', 'ARM64')) {
        $env:PROCESSOR_ARCHITECTURE = 'x86'
        $env:PROCESSOR_ARCHITEW6432 = $native
        ${architecture}
        if ($osArchitecture -ne $native) { throw 'Lost native architecture in an emulated shell' }
      }
      $env:PROCESSOR_ARCHITEW6432 = ''
      $env:PROCESSOR_ARCHITECTURE = 'ARM64'
      ${architecture}
      if ($osArchitecture -ne 'ARM64') { throw 'Lost native shell architecture' }
    `,
      ],
      { encoding: 'utf8' },
    );

    assert.equal(result.status, 0, result.stdout + result.stderr);
  });
}

// Run after cargo build on Windows CI. The proxy replaces only external services:
// the installed wrapper, real CLI update handler, embedded installer, hashing,
// extraction, staging and pointer switch all execute unchanged.
test(
  'Windows installed CLI updates through pwsh and PowerShell 5.1 without replacing active executables',
  { skip: !windows },
  (t) => {
    const cli = process.env.REMEDIA_TEST_CLI;
    if (!cli) return t.skip('Set REMEDIA_TEST_CLI to the built standalone CLI');
    assert.ok(existsSync(cli), cli);

    const box = sandbox();
    const nativeArchitecture =
      process.env.PROCESSOR_ARCHITEW6432 ?? process.env.PROCESSOR_ARCHITECTURE;
    const target =
      nativeArchitecture === 'ARM64' ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
    const realShell = spawnSync(
      'powershell.exe',
      ['-NoProfile', '-Command', '(Get-Command powershell.exe).Source'],
      { encoding: 'utf8' },
    ).stdout.trim();
    const readUserPath = () => {
      const result = spawnSync(
        realShell,
        ['-NoProfile', '-Command', "[Environment]::GetEnvironmentVariable('Path','User')"],
        { encoding: 'utf8' },
      );
      assert.equal(result.status, 0, result.stderr);
      return result.stdout;
    };
    const userPath = readUserPath();
    const psQuote = (value) => `'${value.replaceAll("'", "''")}'`;
    const actualPwsh = spawnSync(
      realShell,
      ['-NoProfile', '-Command', '(Get-Command pwsh).Source'],
      { encoding: 'utf8' },
    );
    assert.equal(actualPwsh.status, 0, actualPwsh.stderr);
    // Keep the Windows/PowerShell module paths needed by the real installer,
    // but hide the real pwsh executable when testing the 5.1 fallback.
    const pwshDir = resolve(dirname(actualPwsh.stdout.trim())).toLowerCase();
    const realPath = process.env.Path ?? process.env.PATH;
    const pathWithoutPwsh = realPath
      .split(';')
      .filter((entry) => resolve(entry.trim().replace(/^"|"$/g, '')).toLowerCase() !== pwshDir)
      .join(';');

    try {
      const payload = join(box.base, 'payload');
      mkdirSync(payload);
      writeFileSync(join(payload, `remedia-server-${target}.exe`), readFileSync(cli));
      writeFileSync(
        join(payload, `remedia-server-runtime-${target}.exe`),
        process.env.REMEDIA_TEST_RUNTIME
          ? readFileSync(process.env.REMEDIA_TEST_RUNTIME)
          : 'fixture runtime',
      );
      for (const name of [
        'LICENSE',
        'THIRD_PARTY_NOTICES.md',
        'THIRD_PARTY_LICENSES.txt',
        'LICENSES.md',
        'COPYING.GPLv3',
      ])
        writeFileSync(join(payload, name), name);
      const archive = join(box.base, 'release.tar.gz');
      packArchive(archive, payload, readdirSync(payload));
      const digest = createHash('sha256').update(readFileSync(archive)).digest('hex');
      const metadata = join(box.base, 'release.json');
      const bootstrap = join(box.base, 'bootstrap.ps1');
      writeFileSync(
        bootstrap,
        `param([string]$Installer, [switch]$NoFFmpeg)
function Invoke-RestMethod { param($Uri, $Headers) Get-Content -LiteralPath ${psQuote(metadata)} -Raw | ConvertFrom-Json }
try {
  & $Installer -NoFFmpeg:$NoFFmpeg
  if (-not $?) { exit 1 }
} catch {
  [Console]::Error.WriteLine($_.ToString())
  exit 1
}
`,
      );
      // A real executable is needed because Rust Command does not execute .cmd files.
      const proxy = join(box.base, 'proxy.rs');
      writeFileSync(
        proxy,
        `use std::{env, fs::OpenOptions, io::Write, process::{Command, exit}};
fn main() {
 let args: Vec<String> = env::args().skip(1).collect();
 let shell = env::var("FIXTURE_SHELL").unwrap();
 let name = env::current_exe().unwrap().file_name().unwrap().to_string_lossy().to_string();
 writeln!(OpenOptions::new().append(true).open(env::var("FIXTURE_LOG").unwrap()).unwrap(), "{} {:?}", name, args).unwrap();
 if let Some(i) = args.iter().position(|a| a == "-File") {
   let mut cmd = Command::new(shell);
   cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]);
   cmd.arg(env::var("FIXTURE_BOOTSTRAP").unwrap()).arg(&args[i+1]);
   cmd.args(&args[i+2..]);
   exit(cmd.status().unwrap().code().unwrap_or(1));
 }
 exit(0);
}
`,
      );
      // Direct rustc invocations do not read Cargo's target linker environment.
      // Use the workflow's MSVC linker rather than Git's link.exe from PATH.
      const linker =
        process.env[`CARGO_TARGET_${target.toUpperCase().replaceAll('-', '_')}_LINKER`];
      const linkerArgs = linker ? ['-C', `linker=${linker}`] : [];
      const compiled = spawnSync(
        'rustc',
        [proxy, ...linkerArgs, '-o', join(box.fakeBin, 'powershell.exe')],
        { encoding: 'utf8' },
      );
      assert.equal(compiled.status, 0, compiled.stderr);
      const proxyBytes = readFileSync(join(box.fakeBin, 'powershell.exe'));
      const env = {
        ...process.env,
        PATH: `${box.fakeBin};${process.env.PATH}`,
        REMEDIA_INSTALL_DIR: box.installDir,
        REMEDIA_ARCHIVE_PATH: archive,
        REMEDIA_NO_MODIFY_PATH: '1',
        FIXTURE_BOOTSTRAP: bootstrap,
        FIXTURE_LOG: box.log,
        FIXTURE_SHELL: realShell,
      };
      const pointer = join(box.installDir, 'libexec/current.txt');
      const setRelease = (version, checksum = digest) =>
        writeFileSync(
          metadata,
          JSON.stringify({
            tag_name: `v${version}`,
            assets: [
              {
                name: `remedia-server-${target}.tar.gz`,
                digest: `sha256:${checksum}`,
              },
            ],
          }),
        );
      const runWrapper = (args, extraEnv = {}) =>
        spawnSync(
          process.env.ComSpec || 'cmd.exe',
          ['/d', '/s', '/c', `""${join(box.installDir, 'bin/remedia.cmd')}" ${args}"`],
          {
            encoding: 'utf8',
            env: { ...env, ...extraEnv },
            // The /S /C command is already correctly quoted for cmd.exe.
            windowsVerbatimArguments: true,
          },
        );

      for (const shell of ['powershell.exe', 'pwsh']) {
        const detected = spawnSync(
          realShell,
          ['-NoProfile', '-Command', `(Get-Command ${shell}).Source`],
          { encoding: 'utf8' },
        );
        assert.equal(detected.status, 0, detected.stderr);
        env.FIXTURE_SHELL = detected.stdout.trim();
        if (shell === 'pwsh') writeFileSync(join(box.fakeBin, 'pwsh.exe'), proxyBytes);
        // Keep module/cmdlet discovery intact while hiding the real pwsh.
        env.PATH = `${box.fakeBin};${pathWithoutPwsh}`;
        setRelease('0.1.0');
        const initial = spawnSync(
          env.FIXTURE_SHELL,
          ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', bootstrap, installPs1, '-NoFFmpeg'],
          { encoding: 'utf8', env },
        );
        assert.equal(initial.status, 0, initial.stdout + initial.stderr);
        assert.ok(
          existsSync(pointer),
          `Installer finished without creating ${pointer}
initial stdout: ${initial.stdout}
initial stderr: ${initial.stderr}
install dir exists: ${existsSync(box.installDir)}
install dir entries: ${existsSync(box.installDir) ? readdirSync(box.installDir).join(', ') : '(missing)'}`,
        );
        const versionProbe = runWrapper('--version');
        assert.equal(
          versionProbe.status,
          0,
          `Installed CLI --version failed with status ${versionProbe.status}
stdout: ${versionProbe.stdout}
stderr: ${versionProbe.stderr}
spawn error: ${versionProbe.error ?? 'none'}
installation: ${box.installDir}
pointer: ${existsSync(pointer) ? readFileSync(pointer, 'utf8') : '(missing)'}`,
        );
        assert.equal(
          spawnSync(
            process.env.ComSpec || 'cmd.exe',
            ['/d', '/s', '/c', `""${join(box.installDir, 'bin/remedia-server.cmd')}" --version"`],
            { env, windowsVerbatimArguments: true },
          ).status,
          0,
        );
        const previous = readFileSync(pointer, 'utf8');
        const oldExe = join(box.installDir, 'libexec', previous, 'remedia-server.exe');
        const oldBytes = readFileSync(oldExe);
        const configFile = join(box.installDir, 'remedia.config.json');
        const configBytes = Buffer.from('{"fixture":"preserve existing configuration"}\n');
        writeFileSync(configFile, configBytes);
        assert.equal(existsSync(join(box.installDir, 'ffmpeg9')), false);
        setRelease('0.2.0');
        // Remove the explicit prefix: update must infer it from its versioned executable.
        const result = runWrapper(shell === 'pwsh' ? 'update' : 'update --no-ffmpeg', {
          REMEDIA_INSTALL_DIR: undefined,
        });
        assert.equal(result.status, 0, result.stdout + result.stderr);
        assert.notEqual(readFileSync(pointer, 'utf8'), previous);
        assert.deepEqual(readFileSync(oldExe), oldBytes);
        assert.deepEqual(readFileSync(configFile), configBytes);
        assert.equal(existsSync(join(box.installDir, 'ffmpeg9')), false);
        assert.match(
          readFileSync(box.log, 'utf8'),
          new RegExp(`${shell.replace('.', '\\.')}.*-File`),
        );
        const current = readFileSync(pointer, 'utf8');
        const repeat = runWrapper('update --no-ffmpeg');
        assert.equal(repeat.status, 0, repeat.stdout + repeat.stderr);
        assert.match(repeat.stdout, /already current/);
        assert.equal(readFileSync(pointer, 'utf8'), current);
        setRelease('0.3.0');
        const missing = runWrapper('update --no-ffmpeg', {
          REMEDIA_ARCHIVE_PATH: join(box.base, 'missing.tar.gz'),
        });
        assert.notEqual(missing.status, 0);
        assert.match(missing.stdout + missing.stderr, /missing.tar.gz/);
        assert.equal(readFileSync(pointer, 'utf8'), current);
        setRelease('0.3.0', '0'.repeat(64));
        const failed = runWrapper('update --no-ffmpeg');
        assert.notEqual(failed.status, 0);
        assert.match(failed.stdout + failed.stderr, /checksum mismatch/i);
        assert.equal(readFileSync(pointer, 'utf8'), current);
        assert.equal(runWrapper('--version').status, 0);
        assert.deepEqual(readFileSync(configFile), configBytes);
        setRelease('0.1.0');
        const switched = runWrapper('upgrade --no-ffmpeg', { REMEDIA_VERSION: '0.1.0' });
        assert.equal(switched.status, 0, switched.stdout + switched.stderr);
        assert.equal(
          readFileSync(
            join(box.installDir, 'libexec', readFileSync(pointer, 'utf8'), 'version.txt'),
            'utf8',
          ),
          'v0.1.0',
        );
        rmSync(join(box.installDir, 'libexec/current.txt'));
      }
    } finally {
      box.cleanup();
      assert.equal(readUserPath(), userPath, 'installer must not modify the user PATH');
    }
  },
);

test('PowerShell platform detection retains native architecture and OS boundaries', (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const source = readFileSync(installPs1, 'utf8');
  const definition = source.slice(
    source.indexOf('function Test-RemediaWindowsPlatform'),
    source.indexOf('$osArchitecture ='),
  );
  const result = spawnSync(
    'pwsh',
    [
      '-NoProfile',
      '-Command',
      `${definition}
    if (!(Test-RemediaWindowsPlatform AMD64 19045 $true)) { exit 1 }
    if (!(Test-RemediaWindowsPlatform ARM64 22000 $true)) { exit 2 }
    if (Test-RemediaWindowsPlatform ARM64 19045 $true) { exit 3 }
    if (Test-RemediaWindowsPlatform x86 22000 $true) { exit 4 }
    if (Test-RemediaWindowsPlatform AMD64 22000 $false) { exit 5 }
  `,
    ],
    { encoding: 'utf8' },
  );
  assert.equal(result.status, 0, result.stderr);
});

test(
  'Unix normalizes legacy paths, retains busy versions, rolls back, and resumes staged installs',
  { skip: !unixInstallerPlatform },
  () => {
    const box = sandbox();
    const libexec = join(box.installDir, 'libexec');
    const pointer = join(libexec, 'current.txt');
    const previous = join(libexec, 'previous.txt');
    const lsof = join(box.fakeBin, 'lsof');
    writeFileSync(
      lsof,
      '#!/bin/sh\ncase "$*" in *"$FIXTURE_BUSY_DIR"*) [ -z "$FIXTURE_BUSY_DIR" ] || { echo 123; exit 0; } ;; esac\nexit 1\n',
    );
    chmodSync(lsof, 0o755);
    try {
      fakeRelease(box, '0.3.0');
      assert.equal(runSh(box, []).status, 0);
      const legacy = 'v0.3.0-tmp.legacy';
      const move = spawnSync('mv', [join(libexec, 'v0.3.0'), join(libexec, legacy)]);
      assert.equal(move.status, 0);
      for (const name of ['version.txt', 'target.txt', 'cli.sha256', 'runtime.sha256'])
        rmSync(join(libexec, legacy, name));
      writeFileSync(pointer, legacy);
      const legacyBytes = readFileSync(join(libexec, legacy, 'remedia-server-runtime'));
      const busy = { FIXTURE_BUSY_DIR: join(libexec, legacy) };
      let result = runSh(box, [], busy);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(readFileSync(pointer, 'utf8'), 'v0.3.0');
      assert.deepEqual(readFileSync(join(libexec, legacy, 'remedia-server-runtime')), legacyBytes);
      result = runSh(box, [], busy);
      assert.equal(result.status, 0, result.stderr);
      assert.ok(existsSync(join(libexec, legacy)));
      assert.equal(runSh(box, []).status, 0);
      assert.equal(existsSync(join(libexec, legacy)), false);
      const stale = join(libexec, '.stage-v0.9.0-2147483647.abandoned');
      mkdirSync(stale);
      writeFileSync(join(stale, 'remedia-server'), 'interrupted copy');
      fakeRelease(box, '0.4.0');
      assert.equal(runSh(box, []).status, 0);
      assert.equal(existsSync(stale), false);
      fakeRelease(box, '0.5.0');
      result = runSh(box, [], { FIXTURE_BUSY_DIR: join(libexec, 'v0.3.0') });
      assert.equal(result.status, 0, result.stderr);
      assert.ok(existsSync(join(libexec, 'v0.3.0')));
      assert.equal(readFileSync(previous, 'utf8'), 'v0.4.0');
      assert.equal(runSh(box, []).status, 0);
      assert.equal(existsSync(join(libexec, 'v0.3.0')), false);
      fakeRelease(box, '0.4.0', { failDownload: true });
      result = runSh(box, []);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(readFileSync(pointer, 'utf8'), 'v0.4.0');
      const pair = spawnSync(join(box.installDir, 'bin/remedia'), ['--pair'], { encoding: 'utf8' });
      assert.equal(pair.status, 0, pair.stderr);
      assert.ok(pair.stdout.includes(join(libexec, 'v0.4.0/remedia-server')));
      assert.match(pair.stdout, /runtime 0\.4\.0/);
      const originalPointer = readFileSync(pointer);
      const originalPrevious = readFileSync(previous);
      // Fail after copying the CLI but before completing its runtime pair.
      const install = join(box.fakeBin, 'install');
      writeFileSync(
        install,
        '#!/bin/sh\nfor last do :; done\ncase "$last" in *remedia-server-runtime) exit 23 ;; esac\nexec /usr/bin/install "$@"\n',
      );
      chmodSync(install, 0o755);
      fakeRelease(box, '0.6.0');
      result = runSh(box, []);
      assert.notEqual(result.status, 0);
      assert.deepEqual(readFileSync(pointer), originalPointer);
      assert.deepEqual(readFileSync(previous), originalPrevious);
      assert.equal(existsSync(join(libexec, 'v0.6.0')), false);
      assert.equal(
        readdirSync(libexec).some((name) => name.startsWith('.stage-')),
        false,
      );
      rmSync(install);
      // A failure replacing current.txt leaves a complete inactive cache, and preserves both pointers.
      const mv = join(box.fakeBin, 'mv');
      writeFileSync(
        mv,
        '#!/bin/sh\nfor last do :; done\ncase "$last" in */current.txt) exit 24 ;; esac\nexec /bin/mv "$@"\n',
      );
      chmodSync(mv, 0o755);
      result = runSh(box, []);
      assert.notEqual(result.status, 0);
      assert.deepEqual(readFileSync(pointer), originalPointer);
      assert.deepEqual(readFileSync(previous), originalPrevious);
      assert.ok(existsSync(join(libexec, 'v0.6.0/remedia-server-runtime')));
      rmSync(mv);
      fakeRelease(box, '0.6.0', { failDownload: true });
      result = runSh(box, []);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(readFileSync(pointer, 'utf8'), 'v0.6.0');
    } finally {
      box.cleanup();
    }
  },
);

test('PowerShell tar fallback selects one executable when PATH contains multiple matches', (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const source = readFileSync(installPs1, 'utf8');
  const start = source.indexOf('function Get-RemediaTar {');
  const end = source.indexOf('# Compute SHA-256', start);
  const result = spawnSync(
    'pwsh',
    [
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      `$ErrorActionPreference = 'Stop'
$env:SystemRoot = ''
function Get-Command { @([pscustomobject]@{ Source = '/first/tar' }, [pscustomobject]@{ Source = '/second/tar' }) }
${source.slice(start, end)}
$resolved = @(Get-RemediaTar)
if ($resolved.Count -ne 1 -or $resolved[0] -cne '/first/tar') { throw "Expected one tar executable, got: $resolved" }`,
    ],
    { encoding: 'utf8' },
  );

  assert.equal(result.status, 0, result.stdout + result.stderr);
});

test('PowerShell normalizes running executable paths before allowing version cleanup', (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const box = sandbox();
  try {
    const source = readFileSync(installPs1, 'utf8');
    const start = source.indexOf('function Test-VersionInUse {');
    const end = source.indexOf('function Remove-ObsoleteVersions {', start);
    const directory = join(box.installDir, 'libexec/v0.3.0');
    mkdirSync(directory, { recursive: true });
    writeFileSync(join(directory, 'remedia-server-runtime.exe'), 'fixture');
    const result = spawnSync(
      'pwsh',
      [
        '-NoProfile',
        '-NonInteractive',
        '-Command',
        `$ErrorActionPreference = 'Stop'
${source.slice(start, end)}
function Get-CimInstance { [pscustomobject]@{ ExecutablePath = $script:executable } }
$directory = '${directory}'
$script:executable = (Split-Path $directory -Parent) + '/./v0.3.0/remedia-server-runtime.exe'
if (!(Test-VersionInUse $directory)) { throw 'Equivalent process path must retain the version' }
$script:executable = $directory + '-other/remedia-server-runtime.exe'
if (Test-VersionInUse $directory) { throw 'A sibling version does not use this directory' }
$script:executable = $null
if (!(Test-VersionInUse $directory)) { throw 'Unknown process paths must retain the version' }`,
      ],
      { encoding: 'utf8' },
    );

    assert.equal(result.status, 0, result.stdout + result.stderr);
  } finally {
    box.cleanup();
  }
});

test('PowerShell normalizes GUID paths, preserves busy versions, and resumes atomic activation', async (t) => {
  if (spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).error)
    return t.skip('pwsh unavailable');
  const box = sandbox();
  const source = readFileSync(installPs1, 'utf8');
  const start = source.indexOf('Write-InstallStatus "Checking GitHub releases');
  const functions = source.slice(source.indexOf('function Write-InstallStatus'), start);
  const workflow = source.slice(start);
  const payload = join(box.base, 'payload');
  const archive = join(box.base, 'release.tar.gz');
  const libexec = join(box.installDir, 'libexec');
  const pointer = join(libexec, 'current.txt');
  const previous = join(libexec, 'previous.txt');
  mkdirSync(payload);
  for (const file of [
    'remedia-server-x86_64-pc-windows-msvc.exe',
    'remedia-server-runtime-x86_64-pc-windows-msvc.exe',
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'THIRD_PARTY_LICENSES.txt',
    'LICENSES.md',
    'COPYING.GPLv3',
  ])
    writeFileSync(join(payload, file), file);
  packArchive(archive, payload, readdirSync(payload));
  const digest = createHash('sha256').update(readFileSync(archive)).digest('hex');
  const invoke = (version, { busy = '', failPointer = false, missingArchive = false } = {}) => {
    const script = join(box.base, 'layout.ps1');
    writeFileSync(
      script,
      `$ErrorActionPreference = 'Stop'
$InstallDir = '${box.installDir}'
$Version = 'latest'
$repo = 'fixture'
$osArchitecture = 'AMD64'
$ArchivePath = '${missingArchive ? join(box.base, 'missing.tar.gz') : archive}'
${failPointer ? functions.replace('function Set-InstallText {', 'function Invoke-OriginalInstallText {') + `\nfunction Set-InstallText { param($Path, $Value) if ((Split-Path $Path -Leaf) -eq 'current.txt') { throw 'activation interrupted' }; Invoke-OriginalInstallText $Path $Value }` : functions}
function Add-RemediaToPath {}
function Get-CimInstance { ${busy ? `return [pscustomobject]@{ ExecutablePath = '${join(busy, 'remedia-server-runtime.exe')}' }` : 'return @()'} }
function Invoke-RestMethod { return @{ tag_name = 'v${version}'; assets = @(@{ name = 'remedia-server-x86_64-pc-windows-msvc.tar.gz'; digest = 'sha256:${digest}'; size = 7 }) } }
${workflow}`,
    );
    return spawnSync('pwsh', ['-NoProfile', '-NonInteractive', '-File', script], {
      encoding: 'utf8',
      env: { ...process.env, SystemRoot: process.env.SystemRoot || join(box.base, 'windows') },
    });
  };
  try {
    let result = invoke('0.3.0');
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(readFileSync(pointer, 'utf8'), 'v0.3.0');
    const legacy = 'v0.3.0-' + 'a'.repeat(32);
    const { renameSync } = await import('node:fs');
    renameSync(join(libexec, 'v0.3.0'), join(libexec, legacy));
    for (const name of ['target.txt', 'cli.sha256', 'runtime.sha256'])
      rmSync(join(libexec, legacy, name));
    writeFileSync(pointer, legacy);
    result = invoke('0.3.0', { busy: join(libexec, legacy) });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(readFileSync(pointer, 'utf8'), 'v0.3.0');
    assert.ok(existsSync(join(libexec, legacy)));
    result = invoke('0.3.0', { busy: join(libexec, legacy) });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.ok(existsSync(join(libexec, legacy)));
    result = invoke('0.3.0');
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(existsSync(join(libexec, legacy)), false);
    const stale = join(libexec, '.stage-v0.9.0-2147483647-' + 'a'.repeat(32));
    mkdirSync(stale);
    for (const version of ['0.4.0', '0.5.0']) {
      result = invoke(version, { busy: join(libexec, 'v0.3.0') });
      assert.equal(result.status, 0, result.stdout + result.stderr);
    }
    assert.equal(existsSync(stale), false);
    assert.ok(existsSync(join(libexec, 'v0.3.0')));
    result = invoke('0.5.0');
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(existsSync(join(libexec, 'v0.3.0')), false);
    result = invoke('0.4.0', { missingArchive: true });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(readFileSync(pointer, 'utf8'), 'v0.4.0');
    assert.equal(readFileSync(previous, 'utf8'), 'v0.5.0');
    const oldPointer = readFileSync(pointer);
    const oldPrevious = readFileSync(previous);
    result = invoke('0.6.0', { failPointer: true });
    assert.notEqual(result.status, 0);
    assert.deepEqual(readFileSync(pointer), oldPointer);
    assert.deepEqual(readFileSync(previous), oldPrevious);
    assert.ok(existsSync(join(libexec, 'v0.6.0/remedia-server-runtime.exe')));
    assert.equal(
      readdirSync(libexec).some((name) => name.startsWith('.stage-')),
      false,
    );
    result = invoke('0.6.0', { missingArchive: true });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(readFileSync(pointer, 'utf8'), 'v0.6.0');
  } finally {
    box.cleanup();
  }
});

test('installed Unix native server starts at its final path and survives updates', async (t) => {
  const cli = process.env.REMEDIA_TEST_CLI;
  const runtime = process.env.REMEDIA_TEST_RUNTIME;
  if (!unixInstallerPlatform || !cli || !runtime)
    return t.skip('native CLI/runtime paths required');
  const box = sandbox();
  const { copyFileSync } = await import('node:fs');
  const nativeVersion = spawnSync(cli, ['--version'], { encoding: 'utf8' })
    .stdout.split('\n')[0]
    .split(' ')[1];
  const nativeRelease = (version) => {
    fakeRelease(box, version);
    const payload = join(box.base, 'release');
    const files = readdirSync(payload);
    copyFileSync(
      cli,
      join(
        payload,
        files.find((name) => /^remedia-server-(?!runtime)/.test(name)),
      ),
    );
    copyFileSync(
      runtime,
      join(
        payload,
        files.find((name) => /^remedia-server-runtime-/.test(name)),
      ),
    );
    const archive = join(box.base, 'release.tar.gz');
    packArchive(archive, payload, files);
    const metadata = join(box.base, 'release.json');
    const release = JSON.parse(readFileSync(metadata, 'utf8'));
    release.assets[0].digest =
      'sha256:' + createHash('sha256').update(readFileSync(archive)).digest('hex');
    writeFileSync(metadata, JSON.stringify(release, null, 2));
  };
  let child;
  let socket;
  try {
    nativeRelease(nativeVersion);
    let result = runSh(box, []);
    assert.equal(result.status, 0, result.stderr);
    const installed = join(box.installDir, 'libexec', 'v' + nativeVersion, 'remedia-server');
    const runtimePath = join(dirname(installed), 'remedia-server-runtime');
    const runtimeBytes = readFileSync(runtimePath);
    const environment = {
      ...process.env,
      HOME: box.home,
      LD_LIBRARY_PATH: '',
      DYLD_LIBRARY_PATH: '',
      REMEDIA_SERVER_RUNTIME: undefined,
    };
    const version = spawnSync(installed, ['--version'], { encoding: 'utf8', env: environment });
    assert.equal(version.status, 0, version.stderr);
    assert.match(version.stdout, /GPL version 3/);
    assert.doesNotMatch(version.stdout, /native runtime could not be loaded/);
    const config = join(box.base, 'server.json');
    writeFileSync(
      config,
      JSON.stringify({
        listen: '127.0.0.1:0',
        users: [{ username: 'installer-test', password: 'fixture-password' }],
        observability: { console: false },
      }),
    );
    child = spawn(installed, ['--config', config, '--shutdown-on-stdin-close'], {
      env: environment,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let errors = '';
    child.stderr.on('data', (data) => {
      errors += data;
    });
    const address = await new Promise((resolveAddress, reject) => {
      let output = '';
      const timer = setTimeout(
        () => reject(new Error('installed server startup timed out: ' + errors)),
        15_000,
      );
      child.on('error', (error) => {
        clearTimeout(timer);
        reject(error);
      });
      child.once('close', (code) => {
        clearTimeout(timer);
        reject(new Error(`installed server exited ${code}: ${errors}`));
      });
      child.stdout.on('data', (data) => {
        output += data;
        const match = /remedia-server listening on ([^\s]+)/.exec(output);
        if (match) {
          clearTimeout(timer);
          resolveAddress(match[1]);
        }
      });
    });
    const { createRequire } = await import('node:module');
    const clientRequire = createRequire(join(root, 'packages/remedia-client/package.json'));
    const WebSocket = clientRequire('ws');
    socket = new WebSocket(`ws://${address}/v1/ws`, {
      headers: {
        Authorization: 'Basic ' + Buffer.from('installer-test:fixture-password').toString('base64'),
        'X-Remedia-Bot-Id': '100000000000000001',
      },
    });
    await new Promise((resolveOpen, reject) => {
      socket.once('open', resolveOpen);
      socket.once('error', reject);
    });
    // The same native bits use fixture release tags to exercise two pointer switches.
    for (const versionTag of ['99.0.1', '99.0.2']) {
      nativeRelease(versionTag);
      result = runSh(box, []);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(child.exitCode, null, errors);
      assert.deepEqual(readFileSync(runtimePath), runtimeBytes);
      assert.equal(socket.readyState, WebSocket.OPEN);
      await new Promise((resolvePong, reject) => {
        const timer = setTimeout(
          () => reject(new Error('existing WebSocket stopped responding')),
          3_000,
        );
        socket.once('pong', () => {
          clearTimeout(timer);
          resolvePong();
        });
        socket.ping();
      });
    }
    socket.close();
    socket = undefined;
    await new Promise((resolveExit, reject) => {
      const timer = setTimeout(() => {
        child.kill();
        reject(new Error('server did not stop after stdin closed'));
      }, 10_000);
      child.once('close', () => {
        clearTimeout(timer);
        resolveExit();
      });
      child.stdin.end();
    });
    child = undefined;
    result = runSh(box, []);
    assert.equal(result.status, 0, result.stderr);
    if (!spawnSync('lsof', ['-v']).error) assert.equal(existsSync(runtimePath), false);
  } finally {
    socket?.terminate();
    child?.kill();
    box.cleanup();
  }
});

test(
  'Unix serializes installers and releases the lock when its owner exits',
  { skip: !unixInstallerPlatform },
  async () => {
    const box = sandbox();
    const libexec = join(box.installDir, 'libexec');
    const lock = join(libexec, '.install.lock');
    mkdirSync(libexec, { recursive: true });
    let holder;
    try {
      fakeRelease(box, '0.3.0');
      const args =
        process.platform === 'darwin' ? ['lockf', '-k', '-t', '0', lock] : ['flock', lock];
      holder = spawn(args[0], [...args.slice(1), 'sh', '-c', 'echo ready; read release'], {
        stdio: ['pipe', 'pipe', 'pipe'],
      });
      await new Promise((resolveReady, reject) => {
        holder.stdout.once('data', resolveReady);
        holder.once('error', reject);
      });
      const result = runSh(box, []);
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, /Another Remedia installer/);
      assert.equal(existsSync(join(libexec, 'current.txt')), false);
      await new Promise((resolveExit) => {
        holder.once('close', resolveExit);
        holder.stdin.end();
      });
      holder = undefined;
      const recovered = runSh(box, []);
      assert.equal(recovered.status, 0, recovered.stderr);
      assert.equal(readFileSync(join(libexec, 'current.txt'), 'utf8'), 'v0.3.0');
    } finally {
      holder?.stdin.end();
      box.cleanup();
    }
  },
);
