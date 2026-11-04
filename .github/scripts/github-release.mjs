import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseReleaseVersion, RELEASE_TARGETS } from './release.mjs';

const noticeFiles = ['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt'];

export function releaseAssets(bindingsDirectory, serversDirectory) {
  const paths = RELEASE_TARGETS.flatMap((target) => [
    join(bindingsDirectory, 'node.' + target.suffix + '.node'),
    ...(target.server === false
      ? []
      : [
          join(serversDirectory, 'remedia-server-' + target.triple + (target.extension ?? '')),
          join(serversDirectory, 'remedia-server-' + target.triple + '.tar.gz'),
        ]),
  ]).concat(noticeFiles.map((file) => join(serversDirectory, file)));
  return paths.map((path) => {
    const bytes = readFileSync(path);
    if (!bytes.length) throw new Error('Empty release asset: ' + path);
    return {
      path,
      name: basename(path),
      size: bytes.length,
      sha256: createHash('sha256').update(bytes).digest('hex'),
    };
  });
}

async function api(path, { method = 'GET', body } = {}) {
  const response = await fetch(
    (process.env.GITHUB_API_URL ?? 'https://api.github.com') +
      '/repos/' +
      process.env.GITHUB_REPOSITORY +
      path,
    {
      method,
      headers: {
        accept: 'application/vnd.github+json',
        authorization: 'Bearer ' + process.env.GH_TOKEN,
        'content-type': 'application/json',
        'X-GitHub-Api-Version': '2022-11-28',
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
    },
  );
  if (!response.ok)
    throw new Error(
      'GitHub release API ' + method + ' ' + path + ' failed: HTTP ' + response.status,
    );
  return response.json();
}

async function getRelease(version, request) {
  parseReleaseVersion(version);
  // The release-by-tag REST endpoint does not reliably find unpublished drafts.
  // Authenticated release listings include drafts and also support safe retries.
  for (let page = 1; ; page++) {
    const releases = await request('/releases?per_page=100&page=' + page);
    const release = releases.find((item) => item.tag_name === 'v' + version);
    if (release) return release;
    if (releases.length < 100) return null;
  }
}

function requireDraft(release) {
  if (!release) throw new Error('Release draft is missing.');
  if (!release.draft) throw new Error('Release is already public; refusing to replace its assets.');
}

export async function prepareDraft(version, sha, request = api) {
  const { prerelease } = parseReleaseVersion(version);
  const existing = await getRelease(version, request);
  if (existing) {
    requireDraft(existing);
    return existing;
  }
  return request('/releases', {
    method: 'POST',
    body: {
      tag_name: 'v' + version,
      target_commitish: sha,
      name: 'Remedia v' + version,
      draft: true,
      prerelease,
      generate_release_notes: true,
    },
  });
}

export async function verifyAssets(release, expected, request = api) {
  const assets = [];
  for (let page = 1; ; page++) {
    const batch = await request('/releases/' + release.id + '/assets?per_page=100&page=' + page);
    assets.push(...batch);
    if (batch.length < 100) break;
  }
  // The expected manifest must cover every configured native target and notice.
  const required = releaseAssetsNames();
  if (
    expected.length !== required.length ||
    required.some((name) => !expected.some((asset) => asset.name === name))
  ) {
    throw new Error('Release asset manifest does not cover all native targets and notices.');
  }
  for (const file of expected) {
    const asset = assets.find((item) => item.name === file.name);
    if (
      !asset ||
      asset.state !== 'uploaded' ||
      asset.size !== file.size ||
      (asset.digest && asset.digest !== 'sha256:' + file.sha256)
    ) {
      throw new Error('Release asset missing, incomplete, or mismatched: ' + file.name);
    }
  }
}

function releaseAssetsNames() {
  return RELEASE_TARGETS.flatMap((target) => [
    'node.' + target.suffix + '.node',
    ...(target.server === false
      ? []
      : [
          'remedia-server-' + target.triple + (target.extension ?? ''),
          'remedia-server-' + target.triple + '.tar.gz',
        ]),
  ]).concat(noticeFiles);
}

export async function uploadDraft(
  version,
  assets,
  {
    request = api,
    upload = (tag, paths) =>
      execFileSync('gh', ['release', 'upload', tag, ...paths, '--clobber'], { stdio: 'inherit' }),
  } = {},
) {
  const release = await getRelease(version, request);
  requireDraft(release);
  await upload(
    'v' + version,
    assets.map((asset) => asset.path),
  );
  await verifyAssets(release, assets, request);
}

export async function finalizeRelease(version, expected, request = api) {
  const { prerelease } = parseReleaseVersion(version);
  const release = await getRelease(version, request);
  if (!release) throw new Error('Release draft is missing.');
  await verifyAssets(release, expected, request);
  // Retrying the final job after a successful publication is harmless.
  if (!release.draft) return;
  return request('/releases/' + release.id, {
    method: 'PATCH',
    body: {
      draft: false,
      prerelease,
      make_latest: prerelease ? 'false' : 'true',
    },
  });
}

async function main() {
  const [command, version, ...args] = process.argv.slice(2);
  if (!process.env.GH_TOKEN || !process.env.GITHUB_REPOSITORY)
    throw new Error('GitHub release credentials and repository are required.');
  if (command === 'prepare') return prepareDraft(version, process.env.GITHUB_SHA);
  if (command === 'upload') {
    const [bindings, servers, manifest] = args;
    const assets = releaseAssets(bindings, servers);
    await uploadDraft(version, assets);
    writeFileSync(
      manifest,
      JSON.stringify(
        assets.map(({ name, size, sha256 }) => ({ name, size, sha256 })),
        null,
        2,
      ) + '\n',
    );
    return;
  }
  if (command === 'finalize')
    return finalizeRelease(version, JSON.parse(readFileSync(args[0], 'utf8')));
  throw new Error('Expected prepare, upload, or finalize command.');
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
