import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, mkdirSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import test from 'node:test';
import {
  snapshotIdentity,
  verifySnapshot,
  verifySourceManifest,
  ensureSnapshot,
  prepareSource,
  publicApi,
} from './corresponding-source.mjs';
import { RELEASE_TARGETS } from './release.mjs';
import { fileHash } from './release-network.mjs';

function fixture() {
  const identity = snapshotIdentity();
  const commit = process.env.GITHUB_SHA ?? 'a'.repeat(40);
  const url = `https://github.com/twlite/remedia/releases/download/sources-${identity}`;
  const sourceTag = 'v1.0.0';
  const release = { id: 7, draft: false, tag_name: `sources-${identity}` };
  const sourceRelease = { id: 8, draft: false, tag_name: sourceTag, target_commitish: commit };
  const assets = [
    {
      name: 'dependencies.tar.gz',
      size: 123,
      state: 'uploaded',
      digest: 'sha256:' + 'a'.repeat(64),
      browser_download_url: url + '/dependencies.tar.gz',
    },
    {
      name: 'corresponding-source.tar.gz',
      size: 50,
      state: 'uploaded',
      digest: 'sha256:' + 'b'.repeat(64),
      browser_download_url: `https://github.com/twlite/remedia/releases/download/${sourceTag}/corresponding-source.tar.gz`,
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
    await assert.rejects(verifySourceManifest(path, f.request), /missing or mismatched/);
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

test('version-specific sources are only exposed through a verified regular release', async () => {
  const root = mkdtempSync(join(tmpdir(), 'remedia-source-release-'));
  const targetsDirectory = join(root, 'targets');
  const records = join(root, 'records');
  const f = fixture();
  const commit = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const release = { id: 8, tag_name: 'v1.0.0', draft: true, target_commitish: commit };
  const publishedAssets = [];
  let publications = 0;
  const request = async (path, options) => {
    if (options?.method) publications++;
    if (path === `/releases/tags/sources-${f.identity}`) return f.release;
    if (path.startsWith('/releases/7/assets?')) return f.assets.slice(0, 1);
    if (path === '/releases/tags/v1.0.0') return release;
    if (path.startsWith('/releases/8/assets?')) return publishedAssets;
    throw new Error('Unexpected release API request: ' + path);
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
    const output = join(root, 'output');
    const manifest = await prepareSource(output, join(root, 'staging'), {
      version: '1.0.0',
      targetsDirectory,
      publish: true,
      request,
      authorize: async () => assert.fail('published snapshot should be reused'),
      build: async () => assert.fail('published snapshot should not be rebuilt'),
      upload: async () => assert.fail('versioned sources must not publish separately'),
    });
    assert.equal(publications, 0, 'preparation does not publish source-only releases');
    assert.equal(manifest.releaseSource.name, 'corresponding-source.tar.gz');
    assert.equal(
      manifest.releaseSource.url,
      'https://github.com/twlite/remedia/releases/download/v1.0.0/corresponding-source.tar.gz',
    );
    const archive = join(output, 'corresponding-source.tar.gz');
    assert.equal(manifest.releaseSource.sha256, await fileHash(archive));
    assert.equal(manifest.releaseSource.size, readFileSync(archive).length);

    const manifestPath = join(output, 'source-manifest.json');
    await assert.rejects(verifySourceManifest(manifestPath, request), /not public/);
    release.draft = false;
    await assert.rejects(verifySourceManifest(manifestPath, request), /missing or mismatched/);
    publishedAssets.push({
      name: manifest.releaseSource.name,
      size: manifest.releaseSource.size,
      state: 'uploaded',
      digest: 'sha256:' + manifest.releaseSource.sha256,
      browser_download_url: manifest.releaseSource.url,
    });
    assert.deepEqual(await verifySourceManifest(manifestPath, request), manifest);
    release.target_commitish = 'b'.repeat(40);
    await assert.rejects(verifySourceManifest(manifestPath, request), /source commit/);
    release.target_commitish = commit;
    publishedAssets[0].digest = 'sha256:wrong';
    await assert.rejects(verifySourceManifest(manifestPath, request), /missing or mismatched/);
    assert.equal(publications, 0);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('public source metadata uses the GitHub token and exposes permission failures', async () => {
  let calls = 0;
  const request = async (url, options) => {
    calls++;
    assert.equal(url, 'https://api.github.com/repos/twlite/remedia/releases/tags/v1.0.0');
    assert.equal(options.headers.authorization, 'Bearer fixture-token');
    assert.equal(options.headers.accept, 'application/vnd.github+json');
    assert.equal(options.headers['X-GitHub-Api-Version'], '2022-11-28');
    return { ok: true, json: async () => ({ tag_name: 'v1.0.0' }) };
  };
  assert.deepEqual(await publicApi('/releases/tags/v1.0.0', { token: 'fixture-token', request }), {
    tag_name: 'v1.0.0',
  });
  assert.equal(calls, 1);
  await assert.rejects(
    publicApi('/releases/tags/v1.0.0', { token: '', request }),
    /GH_TOKEN is required/,
  );
  assert.equal(calls, 1);
  await assert.rejects(
    publicApi('/releases/tags/v1.0.0', {
      token: 'fixture-token',
      request: async () => ({
        ok: false,
        status: 403,
        headers: { get: (key) => (key === 'x-ratelimit-remaining' ? '0' : null) },
        text: async () => '{"message":"API rate limit exceeded"}',
      }),
    }),
    /HTTP 403.*rate-limit remaining: 0.*API rate limit exceeded/,
  );
});
