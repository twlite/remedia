import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { RELEASE_TARGETS, publishRelease } from './release.mjs';

const releaseId = 446659071;
const originalCommit = 'a3a4e876636345ebc2245c2bc26befdc279944f7';

export async function githubGet(path, fetchImpl = fetch) {
  const repository = process.env.GITHUB_REPOSITORY;
  const token = process.env.GH_TOKEN;
  if (!repository || !token) throw new Error('Missing GitHub credentials');
  const response = await fetchImpl('https://api.github.com/repos/' + repository + path, {
    headers: { Authorization: 'Bearer ' + token, Accept: 'application/vnd.github+json' },
    signal: AbortSignal.timeout(20000),
  });
  console.log('GitHub GET ' + path + ': HTTP ' + response.status);
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    const detail =
      response.status === 401 || response.status === 403
        ? 'Insufficient GitHub permissions; unpublished drafts require contents: write.'
        : response.status === 404
          ? 'Release/resource not found or hidden from this token; verify contents: write before treating a draft as missing.'
          : 'GitHub API request failed.';
    throw new Error(
      detail +
        ' GET ' +
        path +
        ': HTTP ' +
        response.status +
        ' (' +
        (body.message ?? 'no API message') +
        ')',
    );
  }
  return response.json();
}

export async function verifyDraft(get = githubGet) {
  const run = await get('/actions/runs/43813499144');
  const tag = await get('/git/ref/tags/v0.3.0');
  if (run.head_sha !== originalCommit || tag.object.sha !== originalCommit)
    throw new Error('Source run and immutable release tag differ from the original commit');
  const release = await get('/releases/' + releaseId);
  console.log(
    'Recovery release identity: ' +
      JSON.stringify({
        id: release.id,
        tag: release.tag_name,
        draft: release.draft,
        commit: release.target_commitish,
      }),
  );
  if (
    release.id !== releaseId ||
    release.tag_name !== 'v0.3.0' ||
    release.target_commitish !== originalCommit
  )
    throw new Error('Release ID, tag, or original commit mismatch');
  if (release.draft !== true)
    throw new Error(
      'v0.3.0 is already published; refusing draft recovery. Retry the failed downstream job instead.',
    );
  const assets = await get('/releases/' + releaseId + '/assets?per_page=100');
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
  if (names.length !== 35 || assets.length !== names.length || new Set(names).size !== names.length)
    throw new Error('Unexpected release asset count or duplicate names');
  const manifest = names.map((name) => {
    const asset = assets.find((item) => item.name === name);
    if (
      !asset ||
      asset.state !== 'uploaded' ||
      !Number.isSafeInteger(asset.size) ||
      asset.size < 1 ||
      !/^sha256:[a-f0-9]{64}$/.test(asset.digest ?? '')
    )
      throw new Error('Missing, incomplete, or unhashed release asset: ' + name);
    return { name, size: asset.size, sha256: asset.digest.slice(7) };
  });
  console.log('Verified ' + manifest.length + ' existing release assets');
  return manifest;
}

export async function verifyNpmPackage(item, fetchImpl = fetch) {
  const response = await fetchImpl(
    'https://registry.npmjs.org/' + encodeURIComponent(item.name) + '/0.3.0',
    {
      signal: AbortSignal.timeout(20000),
    },
  );
  if (response.status === 404) return false;
  if (!response.ok) throw new Error('npm verification HTTP ' + response.status + ': ' + item.name);
  const metadata = await response.json();
  const bytes = readFileSync(item.tarball);
  const integrity = 'sha512-' + createHash('sha512').update(bytes).digest('base64');
  if (
    metadata.name !== item.name ||
    metadata.version !== '0.3.0' ||
    metadata.dist?.integrity !== integrity ||
    metadata.dist?.shasum !== createHash('sha1').update(bytes).digest('hex')
  )
    throw new Error('Published npm package differs from the original artifact: ' + item.name);
  return true;
}

async function main() {
  if (process.argv[2] === 'publish') {
    const manifestPath = resolve(process.argv[3]);
    const packages = JSON.parse(readFileSync(manifestPath, 'utf8')).map((item) => ({
      ...item,
      tarball: join(dirname(manifestPath), item.tarball),
    }));
    const registryCheck = (name) => verifyNpmPackage(packages.find((item) => item.name === name));
    // Fail before publishing anything if an existing version has different bytes.
    for (const item of packages) {
      if (!item.manifest?.private) await verifyNpmPackage(item);
    }
    await publishRelease({ packages, version: '0.3.0', npmTag: 'latest', registryCheck });
    for (const item of packages.filter((candidate) => !candidate.manifest?.private)) {
      if (!(await verifyNpmPackage(item))) throw new Error('npm publication missing: ' + item.name);
    }
    console.log('Verified all published v0.3.0 packages against original tarball hashes');
    return;
  }
  const manifest = await verifyDraft();
  writeFileSync(
    join(process.env.RUNNER_TEMP, 'github-release-assets.json'),
    JSON.stringify(manifest, null, 2) + '\n',
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
}
