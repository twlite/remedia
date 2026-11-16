import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { githubGet, verifyDraft, verifyNpmPackage } from './recover-v030.mjs';
import { RELEASE_TARGETS } from './release.mjs';

const commit = 'a3a4e876636345ebc2245c2bc26befdc279944f7';

function fixture() {
  const release = { id: 446659071, tag_name: 'v0.3.0', target_commitish: commit, draft: true };
  const names = RELEASE_TARGETS.flatMap((target) => [
    'node.' + target.suffix + '.node',
    ...(target.server === false
      ? []
      : [
          'remedia-server-' + target.triple + (target.extension ?? ''),
          'remedia-server-' + target.triple + '.tar.gz',
        ]),
  ]).concat(
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'THIRD_PARTY_LICENSES.txt',
    'LICENSES.md',
    'COPYING.GPLv3',
    'corresponding-source.tar.gz',
  );
  const assets = names.map((name) => ({
    name,
    state: 'uploaded',
    size: 10,
    digest: 'sha256:' + 'a'.repeat(64),
  }));
  const responses = {
    '/actions/runs/43813499144': { head_sha: commit },
    '/git/ref/tags/v0.3.0': { object: { sha: commit } },
    '/releases/446659071': release,
    '/releases/446659071/assets?per_page=100': assets,
  };
  return {
    release,
    assets,
    responses,
    get: async (path) => {
      assert.ok(Object.hasOwn(responses, path), 'Unexpected endpoint: ' + path);
      return responses[path];
    },
  };
}

test('verifies all 35 assets using the exact release ID without listing releases', async () => {
  const manifest = await verifyDraft(fixture().get);
  assert.equal(manifest.length, 35);
  assert.deepEqual(
    manifest.find((item) => item.name === 'corresponding-source.tar.gz'),
    {
      name: 'corresponding-source.tar.gz',
      size: 10,
      sha256: 'a'.repeat(64),
    },
  );
});

for (const [field, value] of [
  ['id', 1],
  ['tag_name', 'v0.3.1'],
  ['target_commitish', 'main'],
  ['draft', false],
]) {
  test('rejects incorrect release ' + field, async () => {
    const data = fixture();
    data.release[field] = value;
    await assert.rejects(
      verifyDraft(data.get),
      field === 'draft' ? /already published/ : /mismatch/,
    );
  });
}

for (const field of ['size', 'digest', 'state']) {
  test('rejects incomplete corresponding source ' + field, async () => {
    const data = fixture();
    data.assets.at(-1)[field] = field === 'size' ? 0 : 'invalid';
    await assert.rejects(verifyDraft(data.get), /corresponding-source/);
  });
}

test('rejects moved original tags and missing assets', async () => {
  const data = fixture();
  data.responses['/git/ref/tags/v0.3.0'].object.sha = 'changed';
  await assert.rejects(verifyDraft(data.get), /immutable release tag/);
  const missing = fixture();
  missing.assets.pop();
  await assert.rejects(verifyDraft(missing.get), /asset count/);
});

test('reports access errors separately and acknowledges hidden-resource 404 ambiguity', async (t) => {
  for (const [name, value] of Object.entries({
    GITHUB_REPOSITORY: 'twlite/remedia',
    GH_TOKEN: 'fixture-token',
  })) {
    const previous = process.env[name];
    process.env[name] = value;
    t.after(() => {
      if (previous === undefined) delete process.env[name];
      else process.env[name] = previous;
    });
  }
  for (const status of [401, 403, 404, 500]) {
    await assert.rejects(
      githubGet(
        '/releases/446659071',
        async () => new Response(JSON.stringify({ message: 'fixture' }), { status }),
      ),
      status === 404
        ? /not found or hidden/
        : status < 500
          ? /Insufficient GitHub permissions/
          : /API request failed/,
    );
  }
});

test('only skips npm versions whose identity and both hashes match the original artifact', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'remedia-recovery-'));
  try {
    const tarball = join(directory, 'fixture.tgz');
    const bytes = Buffer.from('exact original package');
    writeFileSync(tarball, bytes);
    const item = { name: '@remedia/node', tarball };
    const metadata = {
      name: item.name,
      version: '0.3.0',
      dist: {
        integrity: 'sha512-' + createHash('sha512').update(bytes).digest('base64'),
        shasum: createHash('sha1').update(bytes).digest('hex'),
      },
    };
    const response = async () => Response.json(metadata);
    assert.equal(await verifyNpmPackage(item, response), true);
    metadata.dist.integrity = 'wrong';
    await assert.rejects(verifyNpmPackage(item, response), /differs from the original artifact/);
    assert.equal(
      await verifyNpmPackage(item, async () => new Response('', { status: 404 })),
      false,
    );
    await assert.rejects(
      verifyNpmPackage(item, async () => new Response('', { status: 403 })),
      /HTTP 403/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
