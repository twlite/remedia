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

export function snapshotIdentity() {
  return createHash('sha256')
    .update('remedia-dependencies-v1\0')
    .update(readFileSync('Cargo.lock'))
    .update(readFileSync(pinsFile))
    .digest('hex');
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

export async function ensureSnapshot(
  work,
  { publish = false, request = api, inputDirectory } = {},
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
  await authorizeRelease();
  const archive = await buildSnapshot(work, { inputDirectory });
  // Retry after an ambiguous POST failure by looking up the same tag, never making another release blindly.
  let release = existing?.release;
  if (!release) {
    try {
      release = await request('/releases', {
        method: 'POST',
        body: {
          tag_name: `sources-${identity}`,
          target_commitish: process.env.GITHUB_SHA,
          name: `Corresponding dependency sources ${identity.slice(0, 12)}`,
          draft: true,
          prerelease: true,
          body: 'Retained corresponding dependency source for Remedia native binaries. Content identity covers Cargo.lock and source-inputs.json. Do not delete while any associated binary remains distributed. See SOURCE_DISTRIBUTION.md in each binary release.',
        },
      });
    } catch (error) {
      release = (await findSnapshot(identity, request))?.release;
      if (!release) throw error;
    }
  }
  await uploadAssets(release, [archive], { request });
  const uploaded = (await listAssets(release, request)).find(
    (asset) => asset.name === archive.name,
  );
  if (!assetMatches(uploaded, archive)) throw new Error('Source publication digest mismatch');
  await authorizeRelease();
  release = await request('/releases/' + release.id, {
    method: 'PATCH',
    body: { draft: false, prerelease: true, make_latest: 'false' },
  });
  return verifySnapshot(release, await listAssets(release, request), identity);
}

export async function prepareSource(
  destination,
  staging,
  { version, targetsDirectory, publish = false, request = api } = {},
) {
  parseReleaseVersion(version);
  mkdirSync(destination, { recursive: true });
  mkdirSync(staging, { recursive: true });
  const snapshot = await ensureSnapshot(resolve(staging, '../dependency-snapshot'), {
    publish,
    request,
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
    name: `remedia-${version}-${commit}.tar.gz`,
    url: `https://github.com/twlite/remedia/releases/download/sources-${snapshot.identity}/remedia-${version}-${commit}.tar.gz`,
    sha256: await fileHash(archive),
    size: statSync(archive).size,
  };
  if (publish) {
    const existing = await findSnapshot(snapshot.identity, request);
    await authorizeRelease();
    // Append immutable release sources before npm publishes; the binary draft stays private.
    const sourcePath = resolve(staging, '..', manifest.releaseSource.name);
    cpSync(archive, sourcePath);
    await uploadAssets(existing.release, [{ path: sourcePath, ...manifest.releaseSource }], {
      request,
      allowPublished: true,
    });
  }
  writeFileSync(
    join(destination, 'source-manifest.json'),
    JSON.stringify(manifest, null, 2) + '\n',
  );
  console.log(
    `Release sources: ${statSync(archive).size} bytes; dependency snapshot ${snapshot.identity}${snapshot.pending ? ' (dry run; not published)' : ' publicly available'}`,
  );
  return manifest;
}

async function publicApi(path) {
  const response = await requestWithRetry('https://api.github.com/repos/twlite/remedia' + path, {
    headers: { accept: 'application/vnd.github+json' },
  });
  if (!response.ok) {
    const error = new Error(`Public source API ${path}: HTTP ${response.status}`);
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
    manifest.releaseSource?.name !== `remedia-${manifest.version}-${manifest.commit}.tar.gz`
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
  const source = existing.assets.find((asset) => asset.name === manifest.releaseSource?.name);
  if (
    !source ||
    !assetMatches(source, manifest.releaseSource) ||
    source.browser_download_url !== manifest.releaseSource.url
  )
    throw new Error(
      'Release-specific corresponding source is not publicly available or mismatched',
    );
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
