import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path, { dirname } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  assertPackedManifest,
  assertTarContents,
  bootstrapFileName,
  packServerArchives,
  stageReleaseLicenses,
  detectRelease,
  parseReleaseSubject,
  parseReleaseVersion,
  publishRelease,
  registryHasVersion,
  RELEASE_TARGETS,
  syncWorkspaceVersions,
  verifyReleaseArtifacts,
} from './release.mjs';

function temporaryDirectory() {
  return mkdtempSync(path.join(os.tmpdir(), 'remedia-release-test-'));
}

function packageSet(directory, version) {
  const platformPackages = RELEASE_TARGETS.map((target) => {
    const name = '@remedia/node-' + target.suffix;
    const tarball = path.join(directory, target.suffix + '.tgz');
    writeFileSync(tarball, 'fixture');
    return {
      name,
      version,
      kind: 'platform',
      tarball,
      manifest: {
        license: 'MIT',
        name,
        version,
        os: [target.os],
        cpu: [target.cpu],
        ...(target.libc ? { libc: [target.libc] } : {}),
      },
    };
  });
  const optionalDependencies = Object.fromEntries(
    RELEASE_TARGETS.map((target) => ['@remedia/node-' + target.suffix, version]),
  );
  const rootTarball = path.join(directory, 'node.tgz');
  const clientTarball = path.join(directory, 'client.tgz');
  const pluginHostTarball = path.join(directory, 'plugin-host.tgz');
  writeFileSync(pluginHostTarball, 'fixture');
  writeFileSync(rootTarball, 'fixture');
  writeFileSync(clientTarball, 'fixture');
  return [
    ...platformPackages,
    {
      name: '@remedia/node',
      version,
      kind: 'node',
      tarball: rootTarball,
      manifest: {
        license: 'MIT',
        name: '@remedia/node',
        version,
        optionalDependencies,
        files: ['dist', 'native.js', 'native.d.ts'],
      },
    },
    {
      name: '@remedia/plugin-host',
      version,
      kind: 'plugin-host',
      tarball: pluginHostTarball,
      manifest: { license: 'MIT', name: '@remedia/plugin-host', version },
    },
    {
      name: '@remedia/client',
      version,
      kind: 'client',
      tarball: clientTarball,
      manifest: {
        license: 'MIT',
        name: '@remedia/client',
        version,
        peerDependencies: { '@remedia/node': version },
      },
    },
  ];
}

test('the release list exactly matches all configured napi targets', () => {
  const workspaceRoot = path.resolve(dirname(fileURLToPath(import.meta.url)), '../..');
  const manifest = JSON.parse(
    readFileSync(path.join(workspaceRoot, 'packages/remedia-node/package.json'), 'utf8'),
  );
  assert.deepEqual(
    RELEASE_TARGETS.map(({ triple }) => triple),
    manifest.napi.targets,
  );
  assert.equal(RELEASE_TARGETS.length, 13);
  const targetsWithServerBinaries = RELEASE_TARGETS.filter((target) => target.server !== false);
  assert.equal(targetsWithServerBinaries.length, 5);
});

test('accepts the napi-rs ARMv7 GNU package manifest without a libc field', () => {
  const target = RELEASE_TARGETS.find((item) => item.triple === 'armv7-unknown-linux-gnueabihf');
  assert.ok(target);
  assert.equal(target.libc, undefined);
  const manifest = {
    license: 'MIT',
    name: '@remedia/node-linux-arm-gnueabihf',
    version: '0.1.0',
    os: ['linux'],
    cpu: ['arm'],
  };
  assertPackedManifest(manifest, {
    name: manifest.name,
    version: manifest.version,
    kind: 'platform',
  });
});

test('parses stable releases and dynamic lowercase prerelease tags', () => {
  assert.deepEqual(parseReleaseSubject('chore: release 1.2.3'), {
    version: '1.2.3',
    npmTag: 'latest',
    prerelease: false,
  });
  for (const [tag, sequence] of [
    ['alpha', 1],
    ['beta', 2],
    ['rc', 1],
    ['canary', 5],
    ['dev', 12],
    ['nightly', 7],
  ]) {
    const release = parseReleaseSubject('chore: release 1.3.0-' + tag + '.' + sequence);
    assert.equal(release.version, '1.3.0-' + tag + '.' + sequence);
    assert.equal(release.npmTag, tag);
    assert.equal(release.prerelease, true);
  }
});

test('rejects invalid semver, prerelease sequence, and npm dist-tags', () => {
  for (const subject of [
    'chore: release 01.2.3',
    'chore: release 1.2',
    'chore: release 1.2.3-beta',
    'chore: release 1.2.3-beta.01',
    'chore: release 1.2.3-beta.01.1',
    'chore: release 1.2.3-beta_1.1',
    'chore: release 1.2.3-beta..1',
    'chore: release 1.2.3-Latest.1',
    'chore: release 1.2.3-latest.1',
    'chore: release 1.2.3-v1.2.1',
    'chore: release 1.2.3-x.1',
    'chore: release 1.2.3-x.1.2',
    'chore: release 1.2.3-_private.1',
    'chore: release 1.2.3-beta.1+build.4',
    'chore: release 9007199254740992.2.3',
  ]) {
    assert.throws(() => parseReleaseSubject(subject), /Invalid|reserved|SemVer/);
  }
  assert.throws(() => parseReleaseVersion('1.2.3-*.1'), /Invalid release version/);
  assert.throws(() => parseReleaseSubject('chore: release'), /missing its version/);
});

test('only detects complete release subjects on pushes to main', () => {
  assert.throws(
    () => parseReleaseSubject('chore: release 1.2.3 update'),
    /Invalid release version/,
  );
  assert.equal(parseReleaseSubject('feat: release 1.2.3'), null);
  assert.throws(() => parseReleaseSubject('chore: release 1.2.3 '), /Invalid release version/);
  assert.equal(
    detectRelease({
      subject: 'chore: release 1.2.3',
      eventName: 'pull_request',
      ref: 'refs/heads/main',
    }),
    null,
  );
  assert.equal(
    detectRelease({
      subject: 'chore: release 1.2.3',
      eventName: 'push',
      ref: 'refs/heads/feature',
    }),
    null,
  );
  assert.throws(
    () =>
      detectRelease({
        subject: 'chore: release 1.2.3-beta',
        eventName: 'push',
        ref: 'refs/heads/main',
      }),
    /Invalid release version/,
  );
});

test('synchronizes npm workspace versions without rewriting workspace protocols', () => {
  const directory = temporaryDirectory();
  try {
    for (const packagePath of [
      'packages/remedia-node/package.json',
      'packages/remedia-client/package.json',
      'packages/remedia-plugin-host/package.json',
    ]) {
      const fullPath = path.join(directory, packagePath);
      mkdirSync(path.dirname(fullPath), { recursive: true });
      writeFileSync(
        fullPath,
        JSON.stringify(
          packagePath.includes('client')
            ? {
                license: 'MIT',
                name: '@remedia/client',
                version: '0.1.0',
                peerDependencies: { '@remedia/node': 'workspace:*' },
              }
            : packagePath.includes('plugin-host')
              ? { name: '@remedia/plugin-host', version: '0.1.0', private: false }
              : { name: '@remedia/node', version: '0.0.0', private: true },
        ),
      );
    }
    syncWorkspaceVersions('2.4.0-canary.5', directory);
    const nodeManifest = JSON.parse(
      readFileSync(path.join(directory, 'packages/remedia-node/package.json'), 'utf8'),
    );
    const clientManifest = JSON.parse(
      readFileSync(path.join(directory, 'packages/remedia-client/package.json'), 'utf8'),
    );
    const pluginHostManifest = JSON.parse(
      readFileSync(path.join(directory, 'packages/remedia-plugin-host/package.json'), 'utf8'),
    );
    assert.equal(pluginHostManifest.version, '2.4.0-canary.5');
    assert.equal(nodeManifest.version, '2.4.0-canary.5');
    assert.equal(clientManifest.version, '2.4.0-canary.5');
    assert.equal(clientManifest.peerDependencies['@remedia/node'], 'workspace:*');
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('validates packed workspace dependency resolution and required N-API package files', () => {
  assert.throws(
    () =>
      assertPackedManifest(
        {
          license: 'MIT',
          name: '@remedia/client',
          version: '1.0.0',
          peerDependencies: { '@remedia/node': 'workspace:*' },
        },
        { name: '@remedia/client', version: '1.0.0', kind: 'client' },
      ),
    /resolve workspace:\*/,
  );
  assertPackedManifest(
    {
      license: 'MIT',
      name: '@remedia/client',
      version: '1.0.0',
      peerDependencies: { '@remedia/node': '1.0.0' },
    },
    { name: '@remedia/client', version: '1.0.0', kind: 'client' },
  );
  assert.throws(
    () =>
      assertPackedManifest(
        {
          license: 'MIT',
          name: '@remedia/node',
          version: '1.0.0',
          optionalDependencies: {},
          files: ['dist'],
        },
        { name: '@remedia/node', version: '1.0.0', kind: 'node' },
      ),
    /optionalDependencies/,
  );
});

test('matches release server artifacts to the target-triple filenames staged by CI', () => {
  const directory = temporaryDirectory();
  const bindings = path.join(directory, 'bindings');
  const servers = path.join(directory, 'servers');
  mkdirSync(bindings);
  mkdirSync(servers);
  writeFileSync(path.join(bindings, 'native.js'), 'loader');
  writeFileSync(path.join(bindings, 'native.d.ts'), 'declarations');
  for (const target of RELEASE_TARGETS) {
    writeFileSync(path.join(bindings, 'node.' + target.suffix + '.node'), 'addon');
    writeFileSync(path.join(bindings, 'bootstrap.' + target.suffix + '.node'), 'bootstrap');
    if (target.server !== false) {
      writeFileSync(
        path.join(servers, 'remedia-server-' + target.triple + (target.extension ?? '')),
        'server',
      );
      writeFileSync(
        path.join(servers, 'remedia-server-runtime-' + target.triple + (target.extension ?? '')),
        'runtime',
      );
    }
  }

  try {
    assert.equal(verifyReleaseArtifacts(bindings, servers).length, RELEASE_TARGETS.length);
    packServerArchives(servers);
    for (const target of RELEASE_TARGETS.filter((item) => item.server !== false)) {
      const archive = path.join(servers, 'remedia-server-' + target.triple + '.tar.gz');
      const entries = execFileSync('tar', ['-tzf', archive], { encoding: 'utf8' });
      assert.ok(entries.includes('remedia-server-' + target.triple + (target.extension ?? '')));
      assert.ok(
        entries.includes('remedia-server-runtime-' + target.triple + (target.extension ?? '')),
      );
      for (const file of ['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt']) {
        assert.deepEqual(
          execFileSync('tar', ['-xOf', archive, file]),
          readFileSync(path.resolve(file)),
        );
      }
    }
    const requiredServer = path.join(servers, 'remedia-server-' + RELEASE_TARGETS[0].triple);
    rmSync(requiredServer);
    assert.throws(
      () => verifyReleaseArtifacts(bindings, servers),
      /Required verified release artifact/,
    );
    writeFileSync(requiredServer, 'server');

    const requiredRuntime = path.join(
      servers,
      'remedia-server-runtime-' + RELEASE_TARGETS[0].triple,
    );
    rmSync(requiredRuntime);
    assert.throws(
      () => verifyReleaseArtifacts(bindings, servers),
      /Required verified release artifact/,
    );
    writeFileSync(requiredRuntime, 'runtime');

    const addonOnlyTarget = RELEASE_TARGETS.find((target) => target.server === false);
    assert.ok(addonOnlyTarget);
    rmSync(path.join(bindings, 'node.' + addonOnlyTarget.suffix + '.node'));
    assert.throws(
      () => verifyReleaseArtifacts(bindings, servers),
      /Required verified release artifact/,
    );
    writeFileSync(path.join(bindings, 'node.' + addonOnlyTarget.suffix + '.node'), 'addon');

    // Every platform package needs the FFmpeg-free bootstrap next to its engine addon.
    for (const target of RELEASE_TARGETS) {
      const bootstrap = path.join(bindings, 'bootstrap.' + target.suffix + '.node');
      rmSync(bootstrap);
      assert.throws(
        () => verifyReleaseArtifacts(bindings, servers),
        new RegExp('Required verified release artifact is missing: .*bootstrap\\.' + target.suffix),
      );
      writeFileSync(bootstrap, '');
      assert.throws(() => verifyReleaseArtifacts(bindings, servers), /Release artifact is empty/);
      writeFileSync(bootstrap, 'bootstrap');
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('pnpm pack resolves the client workspace peer to the node release version', () => {
  const directory = temporaryDirectory();
  try {
    const workspaceRoot = path.resolve(dirname(fileURLToPath(import.meta.url)), '../..');
    execFileSync('pnpm', ['pack', '--pack-destination', directory], {
      cwd: path.join(workspaceRoot, 'packages/remedia-client'),
      stdio: 'ignore',
    });
    const tarball = path.join(
      directory,
      'remedia-client-' +
        JSON.parse(
          readFileSync(path.join(workspaceRoot, 'packages/remedia-client/package.json'), 'utf8'),
        ).version +
        '.tgz',
    );
    const packed = JSON.parse(
      execFileSync('tar', ['-xOf', tarball, 'package/package.json'], { encoding: 'utf8' }),
    );
    const nodeVersion = JSON.parse(
      readFileSync(path.join(workspaceRoot, 'packages/remedia-node/package.json'), 'utf8'),
    ).version;
    assert.equal(packed.peerDependencies['@remedia/node'], nodeVersion);
    assert.notEqual(packed.peerDependencies['@remedia/node'], 'workspace:*');
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('skips exact versions already on npm and publishes in dependency order', async () => {
  const directory = temporaryDirectory();
  try {
    const packages = packageSet(directory, '1.0.0');
    const existing = new Set([packages[0].name + '@1.0.0']);
    const published = [];
    await publishRelease({
      packages,
      version: '1.0.0',
      npmTag: 'latest',
      registryCheck: async (name, version) => existing.has(name + '@' + version),
      publish: async (tarball, tag) => {
        const item = packages.find((candidate) => candidate.tarball === tarball);
        published.push([item.name, tag]);
        existing.add(item.name + '@' + item.version);
      },
      log: () => {},
    });
    assert.equal(published[0][0], packages[1].name);
    assert.deepEqual(
      published.slice(-3).map(([name]) => name),
      ['@remedia/node', '@remedia/plugin-host', '@remedia/client'],
    );
    assert(published.every(([, tag]) => tag === 'latest'));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('requires a matching plugin-host package before any publication or registry lookup', async () => {
  const directory = temporaryDirectory();
  try {
    for (const mismatch of [false, true]) {
      const packages = packageSet(directory, '1.0.0');
      const index = packages.findIndex((item) => item.name === '@remedia/plugin-host');
      if (mismatch) packages[index].version = '0.9.0';
      else packages.splice(index, 1);
      let touchedRegistry = false;
      await assert.rejects(
        publishRelease({
          packages,
          version: '1.0.0',
          npmTag: 'latest',
          registryCheck: async () => {
            touchedRegistry = true;
            return false;
          },
          publish: async () => {
            touchedRegistry = true;
          },
          log: () => {},
        }),
        /Required package @remedia\/plugin-host@1\.0\.0 is missing or mismatched/,
      );
      assert.equal(touchedRegistry, false);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

for (const interruptedPackage of ['@remedia/plugin-host', '@remedia/client']) {
  test(
    'supports a safe rerun after an interrupted ' + interruptedPackage + ' prerelease',
    async () => {
      const directory = temporaryDirectory();
      try {
        const packages = packageSet(directory, '1.0.0-canary.5');
        const existing = new Set();
        let failed = false;
        const registryCheck = async (name, version) => existing.has(name + '@' + version);
        const publish = async (tarball) => {
          const item = packages.find((candidate) => candidate.tarball === tarball);
          if (item.name === interruptedPackage && !failed) {
            failed = true;
            throw new Error('simulated network interruption');
          }
          existing.add(item.name + '@' + item.version);
        };
        await assert.rejects(
          publishRelease({
            packages,
            version: '1.0.0-canary.5',
            npmTag: 'canary',
            registryCheck,
            publish,
            log: () => {},
          }),
          /simulated network interruption/,
        );
        assert.equal(
          existing.has('@remedia/plugin-host@1.0.0-canary.5'),
          interruptedPackage === '@remedia/client',
        );
        const retried = [];
        await publishRelease({
          packages,
          version: '1.0.0-canary.5',
          npmTag: 'canary',
          registryCheck,
          publish: async (tarball, tag) => {
            const item = packages.find((candidate) => candidate.tarball === tarball);
            retried.push([item.name, tag]);
            existing.add(item.name + '@' + item.version);
          },
          log: () => {},
        });
        assert.deepEqual(
          retried,
          interruptedPackage === '@remedia/plugin-host'
            ? [
                ['@remedia/plugin-host', 'canary'],
                ['@remedia/client', 'canary'],
              ]
            : [['@remedia/client', 'canary']],
        );
      } finally {
        rmSync(directory, { recursive: true, force: true });
      }
    },
  );
}

test('fails closed on registry errors or missing required artifacts', async () => {
  const directory = temporaryDirectory();
  try {
    const packages = packageSet(directory, '1.0.0');
    let published = false;
    await assert.rejects(
      publishRelease({
        packages,
        version: '1.0.0',
        npmTag: 'latest',
        registryCheck: async () => {
          throw new Error('registry unavailable');
        },
        publish: async () => {
          published = true;
        },
        log: () => {},
      }),
      /registry unavailable/,
    );
    assert.equal(published, false);

    await assert.rejects(
      registryHasVersion('@remedia/node', '1.0.0', async () => ({
        status: 503,
        ok: false,
      })),
      /HTTP 503/,
    );
    assert.equal(
      await registryHasVersion('@remedia/node', '1.0.0', async () => ({
        status: 404,
        ok: false,
      })),
      false,
    );

    packages.splice(0, 1);
    await assert.rejects(
      publishRelease({
        packages,
        version: '1.0.0',
        npmTag: 'latest',
        registryCheck: async () => false,
        publish: async () => {},
        log: () => {},
      }),
      /verified platform packages/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('actual workspace tarballs carry exact root licenses and notices', () => {
  const directory = temporaryDirectory();
  const root = path.resolve(dirname(fileURLToPath(import.meta.url)), '../..');
  try {
    for (const name of ['node', 'client', 'plugin-host']) {
      const packageDirectory = path.join(root, 'packages/remedia-' + name);
      execFileSync('pnpm', ['run', name === 'node' ? 'build:types' : 'build'], {
        cwd: packageDirectory,
        stdio: 'ignore',
      });
      execFileSync('pnpm', ['pack', '--pack-destination', directory], {
        cwd: packageDirectory,
        stdio: 'ignore',
      });
      const version = JSON.parse(readFileSync(path.join(packageDirectory, 'package.json'))).version;
      const tarball = path.join(directory, 'remedia-' + name + '-' + version + '.tgz');
      const manifest = JSON.parse(execFileSync('tar', ['-xOf', tarball, 'package/package.json']));
      assert.equal(manifest.license, 'MIT');
      assertTarContents(tarball, manifest, name);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('platform tarballs require all exact license files, including negative cases', () => {
  const directory = temporaryDirectory();
  try {
    const fixture = path.join(directory, 'platform');
    mkdirSync(fixture);
    const manifest = { name: '@remedia/node-darwin-arm64', main: 'node.darwin-arm64.node' };
    writeFileSync(path.join(fixture, 'package.json'), JSON.stringify(manifest));
    writeFileSync(path.join(fixture, manifest.main), 'packaging fixture, not a native binary');
    writeFileSync(path.join(fixture, 'bootstrap.darwin-arm64.node'), 'packaging fixture');
    stageReleaseLicenses(fixture);
    manifest.version = '0.1.0-beta.1';
    writeFileSync(path.join(fixture, 'package.json'), JSON.stringify(manifest));
    const pack = () => {
      execFileSync('pnpm', ['pack', '--pack-destination', directory], {
        cwd: fixture,
        stdio: 'ignore',
      });
      return path.join(directory, 'remedia-node-darwin-arm64-' + manifest.version + '.tgz');
    };
    assertTarContents(pack(), manifest, 'platform');
    rmSync(path.join(fixture, 'bootstrap.darwin-arm64.node'));
    assert.throws(
      () => assertTarContents(pack(), manifest, 'platform'),
      /missing required file package\/bootstrap\.darwin-arm64\.node/,
    );
    writeFileSync(path.join(fixture, 'bootstrap.darwin-arm64.node'), 'packaging fixture');
    rmSync(path.join(fixture, 'THIRD_PARTY_NOTICES.md'));
    assert.throws(() => assertTarContents(pack(), manifest, 'platform'), /missing required file/);
    stageReleaseLicenses(fixture);
    writeFileSync(path.join(fixture, 'LICENSE'), 'incorrect');
    assert.throws(() => assertTarContents(pack(), manifest, 'platform'), /notice differs/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('derives the bootstrap file name from the engine addon file name', () => {
  assert.equal(bootstrapFileName('node.linux-x64-gnu.node'), 'bootstrap.linux-x64-gnu.node');
  assert.equal(bootstrapFileName('node.win32-x64-msvc.node'), 'bootstrap.win32-x64-msvc.node');
  assert.throws(
    () => bootstrapFileName('bootstrap.linux-x64-gnu.node'),
    /Unexpected N-API engine addon name/,
  );
});

test('the bootstrap napi config targets exactly the configured engine targets', () => {
  const root = path.resolve(dirname(fileURLToPath(import.meta.url)), '../..');
  const engine = JSON.parse(
    readFileSync(path.join(root, 'packages/remedia-node/package.json'), 'utf8'),
  );
  const bootstrap = JSON.parse(
    readFileSync(path.join(root, 'packages/remedia-node/bootstrap/napi.json'), 'utf8'),
  );

  assert.deepEqual(bootstrap.targets, engine.napi.targets);
  assert.equal(bootstrap.packageName, engine.napi.packageName);
});
