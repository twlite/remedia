import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { finalizeRelease, prepareDraft, releaseAssets, uploadDraft } from './github-release.mjs';
import { RELEASE_TARGETS } from './release.mjs';

function fixture() {
  const root = mkdtempSync(join(tmpdir(), 'remedia-github-release-'));
  const bindings = join(root, 'bindings');
  const servers = join(root, 'servers');
  mkdirSync(bindings);
  mkdirSync(servers);
  for (const target of RELEASE_TARGETS) {
    writeFileSync(join(bindings, 'node.' + target.suffix + '.node'), 'addon fixture');
    if (target.server !== false) {
      writeFileSync(
        join(servers, 'remedia-server-' + target.triple + (target.extension ?? '')),
        'server fixture',
      );
      writeFileSync(
        join(servers, 'remedia-server-' + target.triple + '.tar.gz'),
        'archive fixture',
      );
    }
  }
  for (const name of ['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt'])
    writeFileSync(join(servers, name), 'notice fixture');
  return { root, assets: releaseAssets(bindings, servers) };
}

function mockApi(assets, { draft = true, version = '1.0.0' } = {}) {
  const calls = [];
  const uploaded = assets.map((asset) => ({
    ...asset,
    state: 'uploaded',
    digest: 'sha256:' + asset.sha256,
  }));
  return {
    calls,
    uploaded,
    request: async (path, options) => {
      calls.push({ path, options });
      if (path.startsWith('/releases?')) return [{ id: 7, draft, tag_name: 'v' + version }];
      if (path.includes('/assets?')) return uploaded;
      if (path === '/releases/7' && options?.method === 'PATCH') return options.body;
      throw new Error('Unexpected request: ' + path);
    },
  };
}

test('prepares a draft before any assets or npm publication, including prerelease metadata', async () => {
  const calls = [];
  const request = async (path, options) => {
    calls.push({ path, options });
    return path.startsWith('/releases?') ? [] : options.body;
  };
  const result = await prepareDraft('0.1.0-beta.1', 'commit-sha', request);
  assert.equal(result.draft, true);
  assert.equal(result.prerelease, true);
  assert.equal(result.target_commitish, 'commit-sha');
  assert.equal(calls[1].options.method, 'POST');
  assert.equal(calls.length, 2);
});

test('reuses failed-run drafts and refuses to demote an existing public release', async () => {
  const existing = { id: 7, draft: true, tag_name: 'v1.0.0' };
  assert.equal(await prepareDraft('1.0.0', 'sha', async () => [existing]), existing);
  await assert.rejects(
    prepareDraft('1.0.0', 'sha', async () => [{ draft: false, tag_name: 'v1.0.0' }]),
    /already public/,
  );
  await assert.rejects(
    prepareDraft('1.0.0', 'sha', async () => {
      throw new Error('HTTP 403');
    }),
    /HTTP 403/,
  );
});

test('uploads every addon, server, archive, and notice while retaining the draft', async () => {
  const { root, assets } = fixture();
  try {
    assert.equal(assets.filter((asset) => asset.name.endsWith('.node')).length, 13);
    assert.equal(assets.length, 26);
    const mock = mockApi(assets, { version: '0.1.0-beta.1' });
    let uploadedPaths;
    await uploadDraft('0.1.0-beta.1', assets, {
      request: mock.request,
      upload: (tag, paths) => {
        assert.equal(tag, 'v0.1.0-beta.1');
        uploadedPaths = paths;
      },
    });
    assert.deepEqual(
      uploadedPaths,
      assets.map((asset) => asset.path),
    );
    assert.ok(mock.calls.every((call) => call.options?.method !== 'PATCH'));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('upload errors fail the stage and public releases never have assets overwritten', async () => {
  const mock = mockApi([]);
  await assert.rejects(
    uploadDraft('1.0.0', [], {
      request: mock.request,
      upload: () => {
        throw new Error('upload failed');
      },
    }),
    /upload failed/,
  );
  assert.equal(mock.calls.length, 1);
  await assert.rejects(
    uploadDraft('1.0.0', [], {
      request: mockApi([], { draft: false }).request,
      upload: () => assert.fail('must not upload'),
    }),
    /already public/,
  );
});

test('missing, partial, corrupt, or incomplete-manifest assets prevent publication', async () => {
  const { root, assets } = fixture();
  try {
    for (const kind of ['missing', 'partial', 'size', 'digest', 'manifest']) {
      const mock = mockApi(assets);
      if (kind === 'missing') mock.uploaded.pop();
      if (kind === 'partial') mock.uploaded[0].state = 'starter';
      if (kind === 'size') mock.uploaded[0].size++;
      if (kind === 'digest') mock.uploaded[0].digest = 'sha256:wrong';
      await assert.rejects(
        finalizeRelease('1.0.0', kind === 'manifest' ? assets.slice(1) : assets, mock.request),
        /asset/,
      );
      assert.ok(mock.calls.every((call) => call.options?.method !== 'PATCH'));
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('finalization selects release or prerelease only after verifying all uploaded assets', async () => {
  const { root, assets } = fixture();
  try {
    for (const [version, prerelease] of [
      ['1.0.0', false],
      ['1.0.0-beta.1', true],
    ]) {
      const mock = mockApi(assets, { version });
      await finalizeRelease(version, assets, mock.request);
      assert.deepEqual(mock.calls.at(-1).options, {
        method: 'PATCH',
        body: { draft: false, prerelease, make_latest: prerelease ? 'false' : 'true' },
      });
    }
    const mock = mockApi(assets, { draft: false });
    await finalizeRelease('1.0.0', assets, mock.request);
    assert.ok(mock.calls.every((call) => call.options?.method !== 'PATCH'));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('finds existing unpublished drafts beyond the first page', async () => {
  const draft = { id: 7, draft: true, tag_name: 'v1.0.0' };
  const calls = [];
  const result = await prepareDraft('1.0.0', 'sha', async (path) => {
    calls.push(path);
    return path.endsWith('page=1')
      ? Array.from({ length: 100 }, (_, i) => ({ tag_name: 'old-' + i }))
      : [draft];
  });
  assert.equal(result, draft);
  assert.equal(calls.length, 2);
});

test('workflow gates npm on verified uploads and public release on successful npm completion', () => {
  const workflow = readFileSync(new URL('../workflows/ci.yaml', import.meta.url), 'utf8');
  const job = (name) => workflow.split('\n  ' + name + ':\n')[1].split(/\n  [a-z][\w-]*:\n/)[0];
  assert.match(job('draft-release'), /needs: release-detection/);
  assert.match(
    job('upload-release'),
    /needs: \[release-detection, draft-release, package-release\]/,
  );
  assert.match(job('publish'), /needs: \[release-detection, package-release, upload-release\]/);
  assert.match(job('github-release'), /needs: \[release-detection, upload-release, publish\]/);
  assert.doesNotMatch(job('github-release'), /always\(\)|continue-on-error/);
});
