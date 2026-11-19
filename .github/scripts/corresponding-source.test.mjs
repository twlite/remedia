import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, mkdirSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname, basename } from 'node:path';
import { tmpdir } from 'node:os';
import test from 'node:test';
import {
  snapshotIdentity,
  verifySnapshot,
  verifySourceManifest,
  ensureSnapshot,
  prepareSource,
  releaseSourceTag,
} from './corresponding-source.mjs';
import { RELEASE_TARGETS } from './release.mjs';
import { uploadAssets } from './github-release.mjs';
import { fileHash } from './release-network.mjs';

function fixture() {
  const identity = snapshotIdentity();
  const commit = process.env.GITHUB_SHA ?? 'a'.repeat(40);
  const url = `https://github.com/twlite/remedia/releases/download/sources-${identity}`;
  const sourceTag = releaseSourceTag('1.0.0', commit);
  const release = { id: 7, draft: false, tag_name: `sources-${identity}` };
  const sourceRelease = { id: 8, draft: false, tag_name: sourceTag };
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
      browser_download_url: `https://github.com/twlite/remedia/releases/download/${sourceTag}/remedia-1.0.0-${commit}.tar.gz`,
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
  const request = async (path) => {
    if (path === `/releases/tags/sources-${identity}`) return release;
    if (path === `/releases/tags/${sourceTag}`) return sourceRelease;
    if (path.startsWith('/releases/7/assets?')) return assets.slice(0, 1);
    if (path.startsWith('/releases/8/assets?')) return assets.slice(1);
    throw new Error('Unexpected request: ' + path);
  };
  return { identity, release, sourceRelease, assets, manifest, request };
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

test('snapshot identity changes with manifests, patches, transformations and archive recipes', () => {
  const root = mkdtempSync(join(tmpdir(), 'remedia-identity-'));
  const inputs = [
    'Cargo.lock',
    'Cargo.toml',
    'crates/remedia-core/Cargo.toml',
    'rust-toolchain.toml',
    '.cargo/config.toml',
    '.github/native-deps/source-inputs.json',
    '.github/native-deps/fixture.patch',
    '.github/scripts/corresponding-source.mjs',
    '.github/scripts/source-archive.py',
    '.github/scripts/prepare-vendor.mjs',
    '.github/scripts/source-checkout.mjs',
    '.github/scripts/patch-ffmpeg-sys-vulkan.mjs',
    '.github/scripts/build-ffmpeg-static.sh',
    '.github/scripts/build-ffmpeg-cross.sh',
    '.github/scripts/build-ffmpeg-windows.ps1',
  ];
  try {
    execFileSync('git', ['init', '-q', root]);
    for (const file of inputs) {
      mkdirSync(dirname(join(root, file)), { recursive: true });
      writeFileSync(join(root, file), 'original input');
    }
    execFileSync('git', ['add', '.'], { cwd: root });
    const original = snapshotIdentity(root);
    const f = fixture();
    for (const file of inputs) {
      writeFileSync(join(root, file), 'changed input');
      const changed = snapshotIdentity(root);
      assert.notEqual(changed, original, file);
      assert.throws(
        () => verifySnapshot({ ...f.release, tag_name: 'sources-' + original }, f.assets, changed),
        /wrong identity/,
        file,
      );
      writeFileSync(join(root, file), 'original input');
      assert.equal(snapshotIdentity(root), original);
    }
    // Newly introduced patch files are inputs too, without extending a fixed file list.
    writeFileSync(join(root, '.github/native-deps/new.patch'), 'new patch');
    execFileSync('git', ['add', '.'], { cwd: root });
    assert.notEqual(snapshotIdentity(root), original);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('release sources use separate immutable releases and are safely reusable', async () => {
  const root = mkdtempSync(join(tmpdir(), 'remedia-source-retry-'));
  const targetsDirectory = join(root, 'targets');
  const records = join(root, 'records');
  const releases = new Map();
  const assets = new Map();
  let releaseId = 7;
  let assetId = 1;
  let builds = 0;
  let creations = 0;
  let authorizations = 0;
  let sourceUploads = 0;
  let mode = 'fail';
  let loseDependencyPublicationResponse = true;
  const identity = snapshotIdentity();
  const dependencyTag = `sources-${identity}`;
  const request = async (path, options) => {
    if (options?.method === 'POST') {
      assert.equal(path, '/releases');
      creations++;
      const release = { id: releaseId++, ...options.body };
      releases.set(release.tag_name, release);
      assets.set(release.id, []);
      return release;
    }
    if (options?.method === 'PATCH') {
      const release = [...releases.values()].find((r) => path === '/releases/' + r.id);
      assert.ok(release);
      Object.assign(release, options.body);
      if (release.tag_name === dependencyTag && loseDependencyPublicationResponse) {
        loseDependencyPublicationResponse = false;
        throw new Error('dependency publication response lost');
      }
      return release;
    }
    if (options?.method === 'DELETE') {
      for (const [id, items] of assets)
        assets.set(
          id,
          items.filter((asset) => path !== '/releases/assets/' + asset.id),
        );
      return null;
    }
    if (path.startsWith('/releases/tags/')) {
      const release = releases.get(path.slice('/releases/tags/'.length));
      if (!release) throw Object.assign(new Error('missing'), { status: 404 });
      return release;
    }
    const assetMatch = path.match(/^\/releases\/(\d+)\/assets\?/);
    if (assetMatch) return assets.get(Number(assetMatch[1])) ?? [];
    if (path.startsWith('/releases?')) return [...releases.values()];
    throw new Error('Unexpected request: ' + path);
  };
  const options = {
    version: '1.0.0',
    targetsDirectory,
    publish: true,
    request,
    authorize: async () => {
      authorizations++;
    },
    build: async (work) => {
      builds++;
      mkdirSync(work, { recursive: true });
      const path = join(work, 'dependencies.tar.gz');
      writeFileSync(path, 'retained dependency source fixture');
      return {
        name: basename(path),
        path,
        size: readFileSync(path).length,
        sha256: await fileHash(path),
      };
    },
    upload: (release, files, uploadOptions) =>
      uploadAssets(release, files, {
        ...uploadOptions,
        wait: async () => {},
        upload: async (tag, [path]) => {
          assert.equal(tag, release.tag_name);
          assert.equal(release.draft, true, 'never upload to an immutable public release');
          const name = basename(path);
          const stored = assets.get(release.id);
          if (name !== 'dependencies.tar.gz') {
            sourceUploads++;
            if (mode === 'fail') {
              stored.push({ id: assetId++, name, state: 'starter' });
              throw new Error('release source connection failed');
            }
            assert.notEqual(mode, 'forbid', 'matching public assets must not be reuploaded');
          }
          stored.push({
            id: assetId++,
            name,
            state: 'uploaded',
            size: readFileSync(path).length,
            digest: 'sha256:' + (await fileHash(path)),
            browser_download_url: `https://github.com/twlite/remedia/releases/download/${tag}/${name}`,
          });
          if (name !== 'dependencies.tar.gz') throw new Error('connection lost after acceptance');
        },
      }),
  };
  try {
    mkdirSync(records);
    mkdirSync(targetsDirectory);
    for (const file of ['config.h', 'config.mak', 'rustc.txt', 'compiler.txt'])
      writeFileSync(join(records, file), 'source record fixture');
    for (const { triple } of RELEASE_TARGETS)
      execFileSync('python3', [
        '.github/scripts/source-archive.py',
        records,
        join(targetsDirectory, `native-source-${triple}.tar.gz`),
      ]);
    const prepare = () => prepareSource(join(root, 'output'), join(root, 'staging'), options);
    await assert.rejects(prepare(), /dependency publication response lost/);
    assert.equal(releases.get(dependencyTag).draft, false);
    assert.equal(builds, 1);
    assert.equal(sourceUploads, 0);
    await assert.rejects(prepare(), /four attempts.*release source connection failed/);
    const commit =
      process.env.GITHUB_SHA ??
      execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
    const sourceTag = releaseSourceTag('1.0.0', commit);
    assert.equal(releases.get(sourceTag).draft, true);
    assert.equal(builds, 1);
    assert.equal(creations, 2);
    assert.equal(sourceUploads, 4);
    const dependencies = structuredClone(assets.get(releases.get(dependencyTag).id));

    mode = 'accept';
    const manifest = await prepare();
    assert.deepEqual(assets.get(releases.get(dependencyTag).id), dependencies);
    assert.equal(releases.get(sourceTag).draft, false);
    assert.equal(sourceUploads, 5, 'ambiguous accepted upload is reconciled without duplication');
    assert.deepEqual(
      await verifySourceManifest(join(root, 'output/source-manifest.json'), request),
      manifest,
    );
    mode = 'forbid';
    assert.deepEqual(await prepare(), manifest);
    assert.equal(builds, 1, 'public dependencies must not be rebuilt on retry');
    assert.equal(creations, 2, 'retry must not create another source release');
    assert.equal(sourceUploads, 5);

    // A new version with the same inputs uses the original public dependency snapshot.
    mode = 'accept';
    options.version = '1.0.1';
    const nextManifest = await prepare();
    assert.equal(releases.get(releaseSourceTag('1.0.1', commit)).draft, false);
    assert.equal(builds, 1);
    assert.equal(creations, 3);
    assert.equal(sourceUploads, 6);
    assert.deepEqual(
      await verifySourceManifest(join(root, 'output/source-manifest.json'), request),
      nextManifest,
    );
    const release = releases.get(releaseSourceTag('1.0.1', commit));
    assets.get(release.id)[0].digest = 'sha256:wrong';
    await assert.rejects(prepare(), /mismatched/);
    assert.equal(sourceUploads, 6);
    assert.ok(authorizations >= 5);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
