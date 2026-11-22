// One retained, content-addressed dependency snapshot; small release-specific sources/records.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
  createWriteStream,
  readdirSync,
  rmSync,
} from 'node:fs';
import { join, resolve, basename } from 'node:path';
import { pathToFileURL } from 'node:url';
import { pipeline } from 'node:stream/promises';
import { api, listAssets, uploadAssets, assetMatches } from './github-release.mjs';
import { fileHash, requestWithRetry, describeError } from './release-network.mjs';
import { RELEASE_TARGETS, parseReleaseVersion } from './release.mjs';
import { authorizeRelease } from './release-authorization.mjs';

const pinsFile = '.github/native-deps/source-inputs.json';
const pins = JSON.parse(readFileSync(pinsFile, 'utf8'));

// v2 binds retained inputs to the recipes that select, transform and archive them.
// The published asset has its own byte digest; this identity describes source compatibility.
export function snapshotIdentity(root = process.cwd()) {
  const files = execFileSync(
    'git',
    [
      'ls-files',
      '-z',
      '--',
      'Cargo.lock',
      'Cargo.toml',
      '**/Cargo.toml',
      'rust-toolchain.toml',
      '.cargo',
      '.github/native-deps',
      '.github/scripts/corresponding-source.mjs',
      '.github/scripts/source-archive.py',
      '.github/scripts/prepare-vendor.mjs',
      '.github/scripts/source-checkout.mjs',
      '.github/scripts/patch-ffmpeg*',
      '.github/scripts/build-ffmpeg*',
    ],
    { cwd: root },
  )
    .toString()
    .split('\0')
    .filter(Boolean)
    .sort();
  const hash = createHash('sha256').update('remedia-dependencies-v2\0');
  for (const file of files) {
    const content = readFileSync(join(root, file));
    hash.update(file).update('\0').update(String(content.length)).update('\0').update(content);
  }
  return hash.digest('hex');
}

export function verifySnapshot(release, assets, identity) {
  if (release.draft || release.tag_name !== `sources-${identity}`)
    throw new Error('Dependency source snapshot is unpublished or has the wrong identity');
  const archive = assets.find((asset) => asset.name === 'dependencies.tar.gz');
  if (
    !archive ||
    archive.state !== 'uploaded' ||
    archive.size <= 0 ||
    !/^sha256:[a-f0-9]{64}$/.test(archive.digest ?? '') ||
    archive.browser_download_url !==
      `https://github.com/twlite/remedia/releases/download/sources-${identity}/dependencies.tar.gz`
  )
    throw new Error('Dependency source snapshot is missing its verified archive');
  return {
    identity,
    url: archive.browser_download_url,
    size: archive.size,
    sha256: archive.digest.slice(7),
  };
}

export async function buildSnapshot(work, { inputDirectory } = {}) {
  mkdirSync(work, { recursive: true });
  const native = join(work, 'native');
  mkdirSync(native, { recursive: true });
  for (const pin of pins) {
    const destination = join(native, `${pin.name}.tar.gz`);
    if (!existsSync(destination)) {
      if (inputDirectory) cpSync(join(inputDirectory, basename(destination)), destination);
      else {
        const response = await requestWithRetry(pin.url);
        if (!response.ok) throw new Error(`Source download ${pin.name}: HTTP ${response.status}`);
        await pipeline(response.body, createWriteStream(destination));
      }
    }
    if (statSync(destination).size !== pin.size || (await fileHash(destination)) !== pin.sha256)
      throw new Error('Source hash mismatch: ' + pin.name);
    // The Windows port's original upstream SHA-512 must match as well.
    if ((await fileHash(destination, 'sha512')) !== pin.sha512)
      throw new Error('Source SHA-512 mismatch: ' + pin.name);
    execFileSync('tar', ['-tzf', destination], { stdio: 'ignore' });
  }
  cpSync(pinsFile, join(work, 'source-inputs.json'));
  cpSync('Cargo.lock', join(work, 'Cargo.lock'));
  const vendor = join(work, 'vendor');
  const config = execFileSync('cargo', ['vendor', '--locked', vendor], {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  // These are dependency test fixtures / unsupported-target objects, not source inputs
  // for any released target. Keep required MSVC System Library import stubs.
  const omitted = [];
  for (const entry of readdirSync(vendor, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const checksumPath = join(vendor, entry.name, '.cargo-checksum.json');
    const checksum = JSON.parse(readFileSync(checksumPath, 'utf8'));
    for (const file of Object.keys(checksum.files)) {
      if (
        /\.(?:dll|exe|o|obj)$/i.test(file) ||
        (/\.(?:a|lib)$/i.test(file) &&
          /^(?:windows_.*_(?:gnu|gnullvm)|winapi-.*-gnu)(?:-|$)/.test(entry.name))
      ) {
        rmSync(join(vendor, entry.name, file));
        delete checksum.files[file];
        omitted.push(`${entry.name}/${file}`);
      }
    }
    writeFileSync(checksumPath, JSON.stringify(checksum));
  }
  writeFileSync(join(work, 'omitted-prebuilt-files.txt'), omitted.sort().join('\n') + '\n');
  writeFileSync(
    join(work, 'vendor-config.toml'),
    config.replaceAll(vendor.replaceAll('\\', '/'), 'vendor'),
  );
  writeFileSync(
    join(work, 'BUILD.txt'),
    `Dependency snapshot ${snapshotIdentity()}\nCombine with the release's corresponding-source.tar.gz and source-manifest.json.\nCopy vendor/ and native/ into the extracted Remedia root, then follow SOURCE_DISTRIBUTION.md.\nNo compiled dependency caches or per-platform source copies are included.\n`,
  );
  const archive = join(resolve(work, '..'), 'dependencies.tar.gz');
  execFileSync('python3', ['.github/scripts/source-archive.py', work, archive]);
  return {
    name: 'dependencies.tar.gz',
    path: archive,
    size: statSync(archive).size,
    sha256: await fileHash(archive),
  };
}

async function findSnapshot(identity, request) {
  // Releases-by-tag returns published snapshots, with no listing of unrelated binary assets.
  try {
    const release = await request(`/releases/tags/sources-${identity}`);
    return { release, assets: await listAssets(release, request) };
  } catch (error) {
    if (error.status === 404) {
      for (let page = 1; ; page++) {
        const releases = await request('/releases?per_page=100&page=' + page);
        const draft = releases.find((item) => item.tag_name === `sources-${identity}`);
        if (draft) return { release: draft, assets: await listAssets(draft, request) };
        if (releases.length < 100) return null;
      }
    }
    throw error;
  }
}

/**
 * Title and description of a snapshot release. The release is addressed by content, so a bare
 * hash says nothing about which Remedia release it serves; both name the version it was built for.
 */
export function snapshotReleaseText(identity, version) {
  const short = identity.slice(0, 12);

  return {
    name: `Corresponding dependency sources for v${version} (${short})`,
    body: `Retained corresponding dependency source for the native binaries of Remedia v${version} (snapshot ${short}). It was first published for v${version}. A later release reuses it unchanged while its pinned inputs, manifests, patches, vendoring and archive recipes stay the same (content identity, format v2). Do not delete while any associated binary remains distributed. See SOURCE_DISTRIBUTION.md in each binary release.`,
  };
}

export async function ensureSnapshot(
  work,
  {
    version,
    publish = false,
    request = api,
    inputDirectory,
    authorize = authorizeRelease,
    build = buildSnapshot,
    upload = uploadAssets,
  } = {},
) {
  const identity = snapshotIdentity();
  const existing = await findSnapshot(identity, request);
  if (existing && !existing.release.draft)
    return verifySnapshot(existing.release, existing.assets, identity);
  if (!publish)
    return {
      identity,
      url: `https://github.com/twlite/remedia/releases/download/sources-${identity}/dependencies.tar.gz`,
      pending: true,
    };
  if (!version) throw new Error('A release version is required to publish a dependency snapshot');
  await authorize();
  const archive = await build(work, { inputDirectory });
  // Retry after an ambiguous POST failure by looking up the same tag, never making another release blindly.
  let release = existing?.release;
  if (!release) {
    try {
      release = await request('/releases', {
        method: 'POST',
        body: {
          tag_name: `sources-${identity}`,
          target_commitish: process.env.GITHUB_SHA,
          ...snapshotReleaseText(identity, version),
          draft: true,
          prerelease: true,
        },
      });
    } catch (error) {
      release = (await findSnapshot(identity, request))?.release;
      if (!release) throw error;
    }
  }
  await upload(release, [archive], { request });
  const uploaded = (await listAssets(release, request)).find(
    (asset) => asset.name === archive.name,
  );
  if (!assetMatches(uploaded, archive)) throw new Error('Source publication digest mismatch');
  await authorize();
  release = await request('/releases/' + release.id, {
    method: 'PATCH',
    body: { draft: false, prerelease: true, make_latest: 'false' },
  });
  return verifySnapshot(release, await listAssets(release, request), identity);
}

// Version-specific sources ship with the regular release, not a separate
// source-only release. npm publication is gated on this release being public.
async function verifyReleaseSource(manifest, request) {
  const tag = `v${manifest.version}`;
  let release;
  try {
    release = await request(`/releases/tags/${tag}`);
  } catch (error) {
    if (error.status === 404)
      throw new Error('Regular release and its corresponding source are not publicly available', {
        cause: error,
      });
    throw error;
  }
  if (
    !release ||
    release.draft ||
    release.tag_name !== tag ||
    release.target_commitish !== manifest.commit
  )
    throw new Error('Regular release is not public or does not match the source commit');
  const source = (await listAssets(release, request)).find(
    (asset) => asset.name === manifest.releaseSource.name,
  );
  if (
    !assetMatches(source, manifest.releaseSource) ||
    source.browser_download_url !== manifest.releaseSource.url
  )
    throw new Error('Regular release corresponding source is missing or mismatched');
  return release;
}

export async function prepareSource(
  destination,
  staging,
  {
    version,
    targetsDirectory,
    publish = false,
    request = api,
    authorize = authorizeRelease,
    build = buildSnapshot,
    upload = uploadAssets,
  } = {},
) {
  parseReleaseVersion(version);
  mkdirSync(destination, { recursive: true });
  mkdirSync(staging, { recursive: true });
  const snapshot = await ensureSnapshot(resolve(staging, '../dependency-snapshot'), {
    version,
    publish,
    request,
    authorize,
    build,
    upload,
  });
  execFileSync('git', ['archive', '--format=tar', '-o', join(staging, 'remedia.tar'), 'HEAD']);
  writeFileSync(join(staging, 'release.patch'), execFileSync('git', ['diff', '--binary', 'HEAD']));
  const commit = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const targets = [];
  for (const { triple } of RELEASE_TARGETS) {
    const name = `native-source-${triple}.tar.gz`;
    const path = join(targetsDirectory, name);
    const entries = execFileSync('tar', ['-tzf', path], { encoding: 'utf8' });
    for (const file of ['config.h', 'config.mak', 'rustc.txt', 'compiler.txt'])
      if (!entries.split('\n').some((entry) => entry === './' + file))
        throw new Error(`Missing target source record ${triple}/${file}`);
    if (/\.(?:o|obj|a|lib|dll|exe)(?:\n|$)/i.test(entries))
      throw new Error('Compiled files in target source records: ' + triple);
    mkdirSync(join(staging, 'targets'), { recursive: true });
    cpSync(path, join(staging, 'targets', name));
    targets.push({ triple, records: `targets/${name}`, sha256: await fileHash(path) });
  }
  execFileSync(process.execPath, [
    '.github/scripts/audit-linked-licenses.mjs',
    join(staging, 'linked-license-inventory.json'),
  ]);
  cpSync('SOURCE_DISTRIBUTION.md', join(staging, 'SOURCE_DISTRIBUTION.md'));
  const manifest = {
    format: 1,
    version,
    commit,
    dependencies: snapshot,
    targets,
    instructions:
      'SOURCE_DISTRIBUTION.md; extract corresponding-source.tar.gz, then remedia.tar and apply release.patch',
  };
  writeFileSync(join(staging, 'source-manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  const archive = join(destination, 'corresponding-source.tar.gz');
  execFileSync('python3', ['.github/scripts/source-archive.py', staging, archive]);
  manifest.releaseSource = {
    name: 'corresponding-source.tar.gz',
    url: `https://github.com/twlite/remedia/releases/download/v${version}/corresponding-source.tar.gz`,
    sha256: await fileHash(archive),
    size: statSync(archive).size,
  };
  writeFileSync(
    join(destination, 'source-manifest.json'),
    JSON.stringify(manifest, null, 2) + '\n',
  );
  console.log(
    `Release sources: ${statSync(archive).size} bytes; dependency snapshot ${snapshot.identity}${snapshot.pending ? ' (dry run; not published)' : ' publicly available'}`,
  );
  return manifest;
}

// Authenticate GitHub API metadata requests to avoid shared-runner anonymous IP
// rate limits. The release itself must still be non-draft and match the manifest.
export async function publicApi(
  path,
  { token = process.env.GH_TOKEN, request = requestWithRetry } = {},
) {
  if (!token) throw new Error('GH_TOKEN is required for corresponding source verification');
  const response = await request('https://api.github.com/repos/twlite/remedia' + path, {
    headers: {
      accept: 'application/vnd.github+json',
      authorization: `Bearer ${token}`,
      'X-GitHub-Api-Version': '2022-11-28',
    },
  });
  if (!response.ok) {
    const body = await response.text();
    const remaining = response.headers?.get('x-ratelimit-remaining');
    const retryAfter = response.headers?.get('retry-after');
    const error = new Error(
      `Public source API ${path}: HTTP ${response.status}` +
        (remaining === null || remaining === undefined
          ? ''
          : ` (rate-limit remaining: ${remaining})`) +
        (retryAfter ? ` (retry after: ${retryAfter}s)` : '') +
        (body ? `: ${body.slice(0, 2048)}` : ''),
    );
    error.status = response.status;
    throw error;
  }
  return response.json();
}

export async function verifySourceManifest(path, request = publicApi) {
  const manifest = JSON.parse(readFileSync(path, 'utf8'));
  if (
    manifest.format !== 1 ||
    !manifest.dependencies ||
    !Array.isArray(manifest.targets) ||
    manifest.dependencies.pending
  )
    throw new Error('Source manifest is incomplete or unpublished');
  parseReleaseVersion(manifest.version);
  if (
    !/^[a-f0-9]{40}$/.test(manifest.commit ?? '') ||
    manifest.releaseSource?.name !== 'corresponding-source.tar.gz' ||
    manifest.releaseSource?.url !==
      `https://github.com/twlite/remedia/releases/download/v${manifest.version}/corresponding-source.tar.gz`
  )
    throw new Error('Release source identity does not match the version and commit');
  if (process.env.GITHUB_SHA && manifest.commit !== process.env.GITHUB_SHA)
    throw new Error('Source manifest commit differs from the release commit');
  if (manifest.dependencies.identity !== snapshotIdentity())
    throw new Error('Dependency source identity differs from build inputs');
  if (
    JSON.stringify(manifest.targets.map((target) => target.triple)) !==
    JSON.stringify(RELEASE_TARGETS.map((target) => target.triple))
  )
    throw new Error('Source manifest does not cover all release targets');
  const existing = await findSnapshot(manifest.dependencies.identity, request);
  if (!existing) throw new Error('Published dependency source snapshot is unavailable');
  const verified = verifySnapshot(
    existing.release,
    existing.assets,
    manifest.dependencies.identity,
  );
  for (const key of ['url', 'sha256', 'size'])
    if (verified[key] !== manifest.dependencies[key])
      throw new Error('Source snapshot metadata mismatch: ' + key);
  await verifyReleaseSource(manifest, request);
  return manifest;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [command, ...args] = process.argv.slice(2);
  const main = async () => {
    if (command === 'snapshot')
      return buildSnapshot(resolve(args[0]), { inputDirectory: process.env.REMEDIA_SOURCE_INPUTS });
    if (command === 'verify') return verifySourceManifest(args[0]);
    if (command === 'prepare')
      return prepareSource(resolve(args[0]), resolve(args[1]), {
        version: args[2],
        targetsDirectory: process.env.REMEDIA_TARGET_SOURCES,
        publish: process.env.REMEDIA_PUBLISH_SOURCES === 'true',
      });
    throw new Error('Expected snapshot, prepare, or verify');
  };
  main().catch((error) => {
    console.error(describeError(error));
    process.exitCode = 1;
  });
}
