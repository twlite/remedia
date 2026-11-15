import { createHash } from 'node:crypto';

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
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
  writeFileSync(binary, `#!/bin/sh\necho 'remedia-server ${version} (commit fixture)'\n`);
  chmodSync(binary, 0o755);
  writeFileSync(join(payload, `remedia-server-runtime-${target}`), 'runtime');
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
      const download = source.slice(
        source.indexOf('    try {\n      Save-InstallDownload -Url $asset.browser_download_url'),
        source.indexOf("  Write-InstallStatus 'Verifying server archive"),
      );
      const script = join(box.base, 'download.ps1');
      const base = `http://127.0.0.1:${server.address().port}`;
      // Execute the production fallback block, including the same HttpClient constructor.
      writeFileSync(
        script,
        `$ErrorActionPreference = 'Stop'\n${functions}\n$archive = '${join(box.base, 'download')}'\nSave-InstallDownload -Url '${base}/direct' -Destination $archive -Label direct -Attempts 1\n$asset = @{ browser_download_url = '${base}/failed'; url = '${base}/asset'; size = 7 }\n${download.slice(0, download.lastIndexOf('\n  }'))}\nif ((Get-Content -Raw $archive) -ne 'archive') { throw 'Bad download' }\n`,
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
    const digest = createHash('sha256').update(readFileSync(archive)).digest('hex');
    const script = join(box.base, 'workflow.ps1');
    const pointer = join(box.installDir, 'libexec/current.txt');
    for (const [version, failure] of [
      ['0.1.0-dev.7', ''],
      ['0.1.0-dev.7', 'skip'],
      ['0.1.0', ''],
      ['0.2.0', 'download'],
      ['0.2.0', 'checksum'],
    ]) {
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
    const realShell = spawnSync(
      'powershell.exe',
      ['-NoProfile', '-Command', '(Get-Command powershell.exe).Source'],
      { encoding: 'utf8' },
    ).stdout.trim();
    const userPath = spawnSync(
      realShell,
      ['-NoProfile', '-Command', "[Environment]::GetEnvironmentVariable('Path','User')"],
      { encoding: 'utf8' },
    ).stdout.trim();
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
      writeFileSync(join(payload, 'remedia-server-x86_64-pc-windows-msvc.exe'), readFileSync(cli));
      writeFileSync(
        join(payload, 'remedia-server-runtime-x86_64-pc-windows-msvc.exe'),
        'fixture runtime',
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
      const linker = process.env.CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER;
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
                name: 'remedia-server-x86_64-pc-windows-msvc.tar.gz',
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
        for (const library of prefixLibraries(box)) {
          mkdirSync(dirname(library), { recursive: true });
          writeFileSync(library, 'active DLL');
        }
        setRelease('0.2.0');
        // Remove the explicit prefix: update must infer it from its versioned executable.
        const result = runWrapper(shell === 'pwsh' ? 'update' : 'update --no-ffmpeg', {
          REMEDIA_INSTALL_DIR: undefined,
        });
        assert.equal(result.status, 0, result.stdout + result.stderr);
        assert.notEqual(readFileSync(pointer, 'utf8'), previous);
        assert.deepEqual(readFileSync(oldExe), oldBytes);
        for (const library of prefixLibraries(box))
          assert.equal(readFileSync(library, 'utf8'), 'active DLL');
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
      spawnSync(realShell, [
        '-NoProfile',
        '-Command',
        `[Environment]::SetEnvironmentVariable('Path', ${psQuote(userPath)}, 'User')`,
      ]);
      box.cleanup();
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
