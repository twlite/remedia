import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import test from 'node:test';
import {
  snapshotIdentity,
  verifySnapshot,
  verifySourceManifest,
  ensureSnapshot,
} from './corresponding-source.mjs';
import { RELEASE_TARGETS } from './release.mjs';

function fixture() {
  const identity = snapshotIdentity();
  const commit = process.env.GITHUB_SHA ?? 'a'.repeat(40);
  const url = `https://github.com/twlite/remedia/releases/download/sources-${identity}`;
  const release = { id: 7, draft: false, tag_name: `sources-${identity}` };
  const assets = [
    {
      name: 'dependencies.tar.gz',
      size: 123,
      state: 'uploaded',
      digest: 'sha256:' + 'a'.repeat(64),
      browser_download_url: url + '/dependencies.tar.gz',
    },
    {
      name: `remedia-1.0.0-${commit}.tar.gz`,
      size: 50,
      state: 'uploaded',
      digest: 'sha256:' + 'b'.repeat(64),
      browser_download_url: url + `/remedia-1.0.0-${commit}.tar.gz`,
    },
  ];
  const manifest = {
    format: 1,
    version: '1.0.0',
    commit,
    dependencies: verifySnapshot(release, assets, identity),
    targets: RELEASE_TARGETS.map(({ triple }) => ({ triple })),
    releaseSource: {
      name: assets[1].name,
      size: 50,
      sha256: 'b'.repeat(64),
      url: assets[1].browser_download_url,
    },
  };
  const request = async (path) => (path.endsWith('/assets?per_page=100&page=1') ? assets : release);
  return { identity, release, assets, manifest, request };
}

test('reuses an existing published dependency snapshot without downloading or rebuilding', async () => {
  const f = fixture();
  assert.deepEqual(
    await ensureSnapshot('/must-not-write', { request: f.request }),
    f.manifest.dependencies,
  );
});

test('draft, missing, unhashed, incomplete and wrong-identity snapshots fail closed', () => {
  const f = fixture();
  for (const [release, assets, identity] of [
    [{ ...f.release, draft: true }, f.assets, f.identity],
    [f.release, [], f.identity],
    [f.release, [{ ...f.assets[0], digest: null }], f.identity],
    [f.release, [{ ...f.assets[0], state: 'starter' }], f.identity],
    [f.release, f.assets, 'wrong'],
  ])
    assert.throws(() => verifySnapshot(release, assets, identity), /snapshot/);
});

test('publication needs complete public source for the exact release and dependency inputs', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'remedia-source-manifest-'));
  const path = join(directory, 'source-manifest.json');
  try {
    const f = fixture();
    writeFileSync(path, JSON.stringify(f.manifest));
    assert.deepEqual(await verifySourceManifest(path, f.request), f.manifest);
    for (const edit of [
      (m) => {
        m.dependencies.pending = true;
      },
      (m) => {
        m.dependencies.identity = 'wrong';
      },
      (m) => {
        m.dependencies.sha256 = 'wrong';
      },
      (m) => {
        m.targets.pop();
      },
      (m) => {
        m.releaseSource.sha256 = 'wrong';
      },
      (m) => {
        m.releaseSource.url = 'https://mutable.example/main';
      },
    ]) {
      const manifest = structuredClone(f.manifest);
      edit(manifest);
      writeFileSync(path, JSON.stringify(manifest));
      await assert.rejects(
        verifySourceManifest(path, f.request),
        /source|snapshot|manifest|identity/i,
      );
    }
    writeFileSync(path, JSON.stringify(f.manifest));
    f.assets.pop();
    await assert.rejects(verifySourceManifest(path, f.request), /not publicly available/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('dry runs mark absent snapshots as pending and never publish', async () => {
  const result = await ensureSnapshot('/must-not-write', {
    request: async (path, options) => {
      assert.equal(options, undefined);
      if (path.startsWith('/releases/tags/'))
        throw Object.assign(new Error('missing'), { status: 404 });
      return [];
    },
  });
  assert.equal(result.pending, true);
  assert.equal(result.identity, snapshotIdentity());
});

test('native recipes and source inventory retain the same exact dependency revisions', async () => {
  const { readFileSync } = await import('node:fs');
  const pins = JSON.parse(readFileSync('.github/native-deps/source-inputs.json', 'utf8'));
  for (const script of ['build-ffmpeg-static.sh', 'build-ffmpeg-cross.sh']) {
    const recipe = readFileSync('.github/scripts/' + script, 'utf8');
    for (const name of ['ffmpeg', 'openssl'])
      assert.ok(
        recipe.includes(pins.find((pin) => pin.name === name).revision),
        `${script}: ${name}`,
      );
  }
  assert.ok(
    readFileSync('.github/scripts/build-ffmpeg-windows.ps1', 'utf8').includes(
      pins.find((pin) => pin.name === 'vcpkg').revision,
    ),
  );
});
