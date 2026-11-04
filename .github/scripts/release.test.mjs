import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path, { dirname } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  assertPackedManifest,
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
        name: '@remedia/node',
        version,
        optionalDependencies,
        files: ['dist', 'native.js', 'native.d.ts'],
      },
    },
    {
      name: '@remedia/client',
      version,
      kind: 'client',
      tarball: clientTarball,
      manifest: {
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
  assert.deepEqual(RELEASE_TARGETS.map(({ triple }) => triple), manifest.napi.targets);
  assert.equal(RELEASE_TARGETS.length, 13);
  assert.equal(RELEASE_TARGETS.filter((target) => target.server !== false).length, 5);
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
    ]) {
      const fullPath = path.join(directory, packagePath);
      mkdirSync(path.dirname(fullPath), { recursive: true });
      writeFileSync(
        fullPath,
        JSON.stringify(
          packagePath.includes('client')
            ? {
                name: '@remedia/client',
                version: '0.1.0',
                peerDependencies: { '@remedia/node': 'workspace:*' },
              }
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
    if (target.server !== false) {
      writeFileSync(
        path.join(servers, 'remedia-server-' + target.triple + (target.extension ?? '')),
        'server',
      );
    }
  }

  try {
    assert.equal(verifyReleaseArtifacts(bindings, servers).length, RELEASE_TARGETS.length);
    const requiredServer = path.join(servers, 'remedia-server-' + RELEASE_TARGETS[0].triple);
    rmSync(requiredServer);
    assert.throws(
      () => verifyReleaseArtifacts(bindings, servers),
      /Required verified release artifact/,
    );
    writeFileSync(requiredServer, 'server');

    const addonOnlyTarget = RELEASE_TARGETS.find((target) => target.server === false);
    assert.ok(addonOnlyTarget);
    rmSync(path.join(bindings, 'node.' + addonOnlyTarget.suffix + '.node'));
    assert.throws(
      () => verifyReleaseArtifacts(bindings, servers),
      /Required verified release artifact/,
    );
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
      published.slice(-2).map(([name]) => name),
      ['@remedia/node', '@remedia/client'],
    );
    assert(published.every(([, tag]) => tag === 'latest'));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('supports a safe rerun after a partially published prerelease', async () => {
  const directory = temporaryDirectory();
  try {
    const packages = packageSet(directory, '1.0.0-canary.5');
    const existing = new Set();
    let failed = false;
    const registryCheck = async (name, version) => existing.has(name + '@' + version);
    const publish = async (tarball) => {
      const item = packages.find((candidate) => candidate.tarball === tarball);
      if (item.name === '@remedia/client' && !failed) {
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
    assert.deepEqual(retried, [['@remedia/client', 'canary']]);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

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
