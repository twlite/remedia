import { authorizeRelease } from './release-authorization.mjs';
import { execFileSync } from 'node:child_process';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const RELEASE_TARGETS = [
  {
    triple: 'x86_64-unknown-linux-gnu',
    suffix: 'linux-x64-gnu',
    os: 'linux',
    cpu: 'x64',
    libc: 'glibc',
  },
  {
    triple: 'aarch64-unknown-linux-gnu',
    suffix: 'linux-arm64-gnu',
    os: 'linux',
    cpu: 'arm64',
    libc: 'glibc',
  },
  {
    triple: 'x86_64-apple-darwin',
    suffix: 'darwin-x64',
    os: 'darwin',
    cpu: 'x64',
  },
  {
    triple: 'aarch64-apple-darwin',
    suffix: 'darwin-arm64',
    os: 'darwin',
    cpu: 'arm64',
  },
  {
    triple: 'x86_64-pc-windows-msvc',
    suffix: 'win32-x64-msvc',
    extension: '.exe',
    os: 'win32',
    cpu: 'x64',
  },
  // Additional publishable N-API targets. The standalone server release
  // currently remains scoped to the five native runner targets above.
  {
    triple: 'x86_64-unknown-linux-musl',
    suffix: 'linux-x64-musl',
    os: 'linux',
    cpu: 'x64',
    libc: 'musl',
    server: false,
  },
  {
    triple: 'aarch64-unknown-linux-musl',
    suffix: 'linux-arm64-musl',
    os: 'linux',
    cpu: 'arm64',
    libc: 'musl',
    server: false,
  },
  {
    triple: 'armv7-unknown-linux-gnueabihf',
    suffix: 'linux-arm-gnueabihf',
    os: 'linux',
    cpu: 'arm',
    // napi-rs does not declare npm libc for this GNU ARMv7 target.
    server: false,
  },
  {
    triple: 'i686-pc-windows-msvc',
    suffix: 'win32-ia32-msvc',
    os: 'win32',
    cpu: 'ia32',
    server: false,
  },
  {
    triple: 'aarch64-pc-windows-msvc',
    suffix: 'win32-arm64-msvc',
    os: 'win32',
    cpu: 'arm64',
    server: false,
  },
  {
    triple: 'aarch64-linux-android',
    suffix: 'android-arm64',
    os: 'android',
    cpu: 'arm64',
    server: false,
  },
  {
    triple: 'armv7-linux-androideabi',
    suffix: 'android-arm-eabi',
    os: 'android',
    cpu: 'arm',
    server: false,
  },
  {
    triple: 'x86_64-unknown-freebsd',
    suffix: 'freebsd-x64',
    os: 'freebsd',
    cpu: 'x64',
    server: false,
  },
];

const workspacePackages = [
  'packages/remedia-node/package.json',
  'packages/remedia-client/package.json',
];
const npmName = '@remedia/node';
const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), '../..');

export function parseReleaseVersion(version) {
  const stable = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(version);
  if (stable && stable.slice(1).every((part) => Number.isSafeInteger(Number(part)))) {
    return { version, npmTag: 'latest', prerelease: false };
  }

  const pre = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)$/.exec(
    version,
  );
  if (!pre) {
    throw new Error(
      'Invalid release version "' +
        version +
        '". Use x.y.z or x.y.z-tag.n with canonical numeric components and a lowercase tag.',
    );
  }

  const prereleaseIdentifiers = pre[4].split('.');
  const sequence = prereleaseIdentifiers.pop();
  const npmTag = prereleaseIdentifiers.join('.');
  if (!/^(0|[1-9]\d*)$/.test(sequence)) {
    throw new Error(
      'Invalid release version "' +
        version +
        '": the prerelease must end in a canonical numeric sequence, such as -beta.2.',
    );
  }
  if (
    prereleaseIdentifiers.some(
      (identifier) => /^\d+$/.test(identifier) && identifier.length > 1 && identifier[0] === '0',
    )
  ) {
    throw new Error('Invalid SemVer prerelease identifier in release version "' + version + '".');
  }
  if (!/^[a-z][a-z0-9-]*(?:\.(?:0|[1-9]\d*|(?=[a-z0-9-]*[a-z-])[a-z0-9-]+))*$/.test(npmTag)) {
    throw new Error('Invalid npm dist-tag "' + npmTag + '". Use lowercase URL-safe characters.');
  }
  if (npmTag === 'latest') {
    throw new Error('The npm dist-tag "latest" is reserved for stable releases.');
  }
  if (/^(?:v(?=\d)|x$|x\.(?:x|\d+)(?:\.(?:x|\d+))?$)/.test(npmTag)) {
    throw new Error(
      'Invalid npm dist-tag "' + npmTag + '": npm interprets it as a SemVer version or range.',
    );
  }
  return { version, npmTag, prerelease: true };
}

export function parseReleaseSubject(subject) {
  const prefix = 'chore: release ';
  if (subject === 'chore: release') {
    throw new Error(
      'Release subject is missing its version; expected chore: release x.y.z or chore: release x.y.z-tag.n.',
    );
  }
  if (!subject.startsWith(prefix)) return null;
  if (subject.includes('\n') || subject.includes('\r')) {
    throw new Error('Release subject must be a single line matching the complete release format.');
  }
  return parseReleaseVersion(subject.slice(prefix.length));
}

export function detectRelease({ subject, eventName, ref }) {
  if (!['push', 'workflow_dispatch'].includes(eventName) || ref !== 'refs/heads/main') return null;
  return parseReleaseSubject(subject);
}

export function syncWorkspaceVersions(version, workspaceRoot = rootDir) {
  parseReleaseVersion(version);
  for (const relativePath of workspacePackages) {
    const manifestPath = join(workspaceRoot, relativePath);
    if (!existsSync(manifestPath))
      throw new Error('Missing npm workspace manifest: ' + manifestPath);
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    manifest.version = version;
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
  }
}

export function assertPackedManifest(manifest, { name, version, kind }) {
  if (manifest.name !== name) {
    throw new Error(
      'Packed package name mismatch: expected ' + name + ', received ' + manifest.name,
    );
  }
  if (manifest.version !== version) {
    throw new Error(
      'Packed ' +
        name +
        ' version mismatch: expected ' +
        version +
        ', received ' +
        manifest.version,
    );
  }
  if (manifest.license !== 'MIT') throw new Error(name + ' must declare license MIT.');
  if (manifest.private === true)
    throw new Error('Refusing to publish private package ' + name + '.');

  if (kind === 'platform') {
    const target = RELEASE_TARGETS.find((item) => name === npmName + '-' + item.suffix);
    if (!target) throw new Error('Unverified platform package is not publishable: ' + name + '.');
    if (JSON.stringify(manifest.os) !== JSON.stringify([target.os])) {
      throw new Error(name + ' must restrict os to ' + target.os + '.');
    }
    if (JSON.stringify(manifest.cpu) !== JSON.stringify([target.cpu])) {
      throw new Error(name + ' must restrict cpu to ' + target.cpu + '.');
    }
    if (target.libc && JSON.stringify(manifest.libc) !== JSON.stringify([target.libc])) {
      throw new Error(name + ' must restrict libc to ' + target.libc + '.');
    }
    if (!target.libc && manifest.libc !== undefined) {
      throw new Error(name + ' must not declare a Linux libc restriction.');
    }
  }

  if (name === '@remedia/node') {
    const expectedDependencies = RELEASE_TARGETS.map(({ suffix }) => npmName + '-' + suffix);
    const actualDependencies = Object.keys(manifest.optionalDependencies ?? {}).filter(
      (dependency) => dependency.startsWith(npmName + '-'),
    );
    if (
      actualDependencies.length !== expectedDependencies.length ||
      expectedDependencies.some((dependency) => !actualDependencies.includes(dependency))
    ) {
      throw new Error(
        name + ' optionalDependencies must contain only the verified release platform packages.',
      );
    }
    for (const { suffix } of RELEASE_TARGETS) {
      const dependency = npmName + '-' + suffix;
      if (manifest.optionalDependencies?.[dependency] !== version) {
        throw new Error(name + ' must depend on ' + dependency + '@' + version + '.');
      }
    }
    for (const path of ['dist/index.js', 'dist/index.d.ts', 'native.js', 'native.d.ts']) {
      if (!manifest.files?.includes(path.split('/')[0]) && !manifest.files?.includes(path)) {
        throw new Error(name + ' package files do not include ' + path + '.');
      }
    }
  }

  if (name === '@remedia/client' && manifest.peerDependencies?.[npmName] !== version) {
    throw new Error(
      name +
        ' packed peer dependency must resolve workspace:* to ' +
        version +
        '; received ' +
        (manifest.peerDependencies?.[npmName] ?? '(missing)') +
        '.',
    );
  }
}

export async function registryHasVersion(name, version, fetchImpl = fetch) {
  const encodedName = name.replace('/', '%2f');
  const url = 'https://registry.npmjs.org/' + encodedName + '/' + encodeURIComponent(version);
  let response;
  try {
    response = await fetchImpl(url, { headers: { accept: 'application/json' } });
  } catch (error) {
    throw new Error('Could not query npm registry for ' + name + '@' + version + '.', {
      cause: error,
    });
  }

  if (response.status === 404) return false;
  if (!response.ok) {
    throw new Error(
      'npm registry lookup for ' +
        name +
        '@' +
        version +
        ' failed with HTTP ' +
        response.status +
        '.',
    );
  }

  let metadata;
  try {
    metadata = await response.json();
  } catch (error) {
    throw new Error('npm registry returned invalid metadata for ' + name + '@' + version + '.', {
      cause: error,
    });
  }
  if (metadata.name !== name || metadata.version !== version) {
    throw new Error('npm registry returned mismatched metadata for ' + name + '@' + version + '.');
  }
  return true;
}

function readTarManifest(tarball) {
  const manifestJson = execFileSync('tar', ['-xOf', tarball, 'package/package.json'], {
    encoding: 'utf8',
  });
  return JSON.parse(manifestJson);
}

export function assertTarContents(tarball, manifest, kind) {
  const entries = execFileSync('tar', ['-tzf', tarball], { encoding: 'utf8' }).trim().split('\n');
  const required =
    kind === 'platform'
      ? ['package/' + manifest.main]
      : kind === 'node'
        ? [
            'package/dist/index.js',
            'package/dist/index.d.ts',
            'package/native.js',
            'package/native.d.ts',
            'package/README.md',
          ]
        : ['package/dist/index.js', 'package/dist/index.d.ts', 'package/README.md'];
  required.push(
    'package/LICENSE',
    'package/THIRD_PARTY_NOTICES.md',
    'package/THIRD_PARTY_LICENSES.txt',
  );
  for (const file of required) {
    if (!entries.includes(file)) {
      throw new Error('Packed ' + manifest.name + ' is missing required file ' + file + '.');
    }
  }
  for (const file of ['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt']) {
    const actual = execFileSync('tar', ['-xOf', tarball, 'package/' + file]);
    if (!actual.equals(readFileSync(join(rootDir, file))))
      throw new Error('Packed notice differs: ' + file);
  }
}

function run(command, args, cwd = rootDir) {
  execFileSync(command, args, { cwd, stdio: 'inherit' });
}

function packPackage(packageDirectory, outputDirectory) {
  mkdirSync(outputDirectory, { recursive: true });
  const before = new Set(readdirSync(outputDirectory));
  run('pnpm', ['pack', '--pack-destination', outputDirectory], packageDirectory);
  const created = readdirSync(outputDirectory).filter(
    (entry) => entry.endsWith('.tgz') && !before.has(entry),
  );
  if (created.length !== 1) {
    throw new Error(
      'Expected pnpm pack to create one tarball for ' +
        packageDirectory +
        ', found ' +
        created.length +
        '.',
    );
  }
  return join(outputDirectory, created[0]);
}

export function verifyReleaseArtifacts(bindingsDirectory, serversDirectory) {
  const artifacts = [];
  for (const generated of ['native.js', 'native.d.ts']) {
    const generatedPath = join(bindingsDirectory, generated);
    if (!existsSync(generatedPath) || readFileSync(generatedPath).byteLength === 0) {
      throw new Error('Generated N-API release file is missing or empty: ' + generatedPath);
    }
  }
  for (const target of RELEASE_TARGETS) {
    const addon = join(bindingsDirectory, 'node.' + target.suffix + '.node');
    const server =
      target.server === false
        ? undefined
        : join(serversDirectory, 'remedia-server-' + target.triple) + (target.extension ?? '');
    const serverRuntime =
      target.server === false
        ? undefined
        : join(serversDirectory, 'remedia-server-runtime-' + target.triple) +
          (target.extension ?? '');
    for (const required of [addon, ...(server ? [server, serverRuntime] : [])]) {
      if (!existsSync(required))
        throw new Error('Required verified release artifact is missing: ' + required);
      if (readFileSync(required).byteLength === 0)
        throw new Error('Release artifact is empty: ' + required);
    }
    artifacts.push({ ...target, addon, ...(server ? { server, serverRuntime } : {}) });
  }
  return artifacts;
}

export function stageReleaseLicenses(directory, workspaceRoot = rootDir) {
  mkdirSync(directory, { recursive: true });
  for (const file of ['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt']) {
    copyFileSync(join(workspaceRoot, file), join(directory, file));
  }
}

export function packServerArchives(serversDirectory, workspaceRoot = rootDir) {
  stageReleaseLicenses(serversDirectory, workspaceRoot);
  for (const target of RELEASE_TARGETS.filter((item) => item.server !== false)) {
    const binary = 'remedia-server-' + target.triple + (target.extension ?? '');
    const runtime = 'remedia-server-runtime-' + target.triple + (target.extension ?? '');
    const archive = join(serversDirectory, 'remedia-server-' + target.triple + '.tar.gz');
    execFileSync('tar', [
      '-czf',
      archive,
      '-C',
      serversDirectory,
      binary,
      runtime,
      'LICENSE',
      'THIRD_PARTY_NOTICES.md',
      'THIRD_PARTY_LICENSES.txt',
    ]);
    for (const file of ['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt']) {
      const actual = execFileSync('tar', ['-xOf', archive, file]);
      if (!actual.equals(readFileSync(join(workspaceRoot, file)))) {
        throw new Error('Server archive has mismatched ' + file + ': ' + archive);
      }
    }
  }
}

export function packRelease(
  { version, bindingsDirectory, serversDirectory, outputDirectory },
  workspaceRoot = rootDir,
) {
  parseReleaseVersion(version);
  const artifactSet = verifyReleaseArtifacts(bindingsDirectory, serversDirectory);
  packServerArchives(serversDirectory, workspaceRoot);
  const nodeManifestPath = join(workspaceRoot, 'packages/remedia-node/package.json');
  const nodeManifest = JSON.parse(readFileSync(nodeManifestPath, 'utf8'));
  // Release packaging must not silently drop any configured napi-rs target.
  const configuredTargets = nodeManifest.napi?.targets;
  if (
    JSON.stringify(configuredTargets) !==
    JSON.stringify(RELEASE_TARGETS.map(({ triple }) => triple))
  ) {
    throw new Error('RELEASE_TARGETS must exactly match @remedia/node napi.targets');
  }
  nodeManifest.version = version;
  nodeManifest.private = false;
  writeFileSync(nodeManifestPath, JSON.stringify(nodeManifest, null, 2) + '\n');
  for (const generated of ['native.js', 'native.d.ts']) {
    copyFileSync(
      join(bindingsDirectory, generated),
      join(workspaceRoot, 'packages/remedia-node', generated),
    );
  }
  const clientManifestPath = join(workspaceRoot, 'packages/remedia-client/package.json');
  const clientManifest = JSON.parse(readFileSync(clientManifestPath, 'utf8'));
  clientManifest.version = version;
  writeFileSync(clientManifestPath, JSON.stringify(clientManifest, null, 2) + '\n');

  run('pnpm', ['--filter', '@remedia/node', 'exec', 'napi', 'create-npm-dirs'], workspaceRoot);
  run('pnpm', ['--filter', '@remedia/node', 'exec', 'napi', 'version'], workspaceRoot);
  run(
    'pnpm',
    ['--filter', '@remedia/node', 'exec', 'napi', 'artifacts', '--output-dir', bindingsDirectory],
    workspaceRoot,
  );
  run(
    'pnpm',
    [
      '--filter',
      '@remedia/node',
      'exec',
      'napi',
      'pre-publish',
      '--no-gh-release',
      '--skip-optional-publish',
      '--root-publisher',
      'pnpm',
    ],
    workspaceRoot,
  );
  run('pnpm', ['--filter', '@remedia/node', 'build:types'], workspaceRoot);
  run('pnpm', ['--filter', '@remedia/client', 'build'], workspaceRoot);

  const platformDirectory = join(workspaceRoot, 'packages/remedia-node/npm');
  const packed = [];
  for (const target of artifactSet) {
    const directory = join(platformDirectory, target.suffix);
    stageReleaseLicenses(directory, workspaceRoot);
    const platformManifestPath = join(directory, 'package.json');
    const platformManifest = JSON.parse(readFileSync(platformManifestPath, 'utf8'));
    platformManifest.license = 'MIT';
    platformManifest.files = [
      ...new Set([
        ...(platformManifest.files ?? [platformManifest.main]),
        'LICENSE',
        'THIRD_PARTY_NOTICES.md',
        'THIRD_PARTY_LICENSES.txt',
      ]),
    ];
    writeFileSync(platformManifestPath, JSON.stringify(platformManifest, null, 2) + '\n');
    const tarball = packPackage(directory, outputDirectory);
    const manifest = readTarManifest(tarball);
    assertPackedManifest(manifest, {
      name: npmName + '-' + target.suffix,
      version,
      kind: 'platform',
    });
    assertTarContents(tarball, manifest, 'platform');
    packed.push({
      name: manifest.name,
      version,
      kind: 'platform',
      tarball: basename(tarball),
      manifest,
    });
  }

  for (const [name, kind, directory] of [
    [npmName, 'node', join(workspaceRoot, 'packages/remedia-node')],
    ['@remedia/client', 'client', join(workspaceRoot, 'packages/remedia-client')],
  ]) {
    const tarball = packPackage(directory, outputDirectory);
    const manifest = readTarManifest(tarball);
    assertPackedManifest(manifest, { name, version, kind });
    assertTarContents(tarball, manifest, kind);
    packed.push({ name, version, kind, tarball: basename(tarball), manifest });
  }

  const packageList = join(outputDirectory, 'release-packages.json');
  writeFileSync(packageList, JSON.stringify(packed, null, 2) + '\n');
  return packageList;
}

export async function publishRelease({
  packages,
  version,
  npmTag,
  registryCheck = registryHasVersion,
  publish = publishTarball,
  log = console.log,
}) {
  const parsed = parseReleaseVersion(version);
  if (parsed.npmTag !== npmTag) {
    throw new Error(
      'Release tag mismatch: ' + version + ' requires npm dist-tag ' + parsed.npmTag + '.',
    );
  }
  const platformPackages = packages.filter((item) => item.kind === 'platform');
  const rootPackage = packages.find((item) => item.name === npmName);
  const clientPackage = packages.find((item) => item.name === '@remedia/client');

  if (platformPackages.length !== RELEASE_TARGETS.length) {
    throw new Error(
      'Expected exactly ' +
        RELEASE_TARGETS.length +
        ' verified platform packages, received ' +
        platformPackages.length +
        '.',
    );
  }
  for (const target of RELEASE_TARGETS) {
    const name = npmName + '-' + target.suffix;
    const item = platformPackages.find((candidate) => candidate.name === name);
    if (!item || item.version !== version) {
      throw new Error(
        'Required platform package ' + name + '@' + version + ' is missing or mismatched.',
      );
    }
  }
  for (const item of [rootPackage, clientPackage]) {
    if (!item || item.version !== version) {
      throw new Error(
        'Required package ' +
          (item?.name ?? '(unknown)') +
          '@' +
          version +
          ' is missing or mismatched.',
      );
    }
  }

  const ordered = [
    ...RELEASE_TARGETS.map((target) =>
      platformPackages.find((item) => item.name === npmName + '-' + target.suffix),
    ),
    rootPackage,
    clientPackage,
  ];
  const publishedThisRun = new Set();
  for (const item of ordered) {
    if (item.manifest?.private === true) {
      log('Skipping intentionally private package ' + item.name + '.');
      continue;
    }
    if (!existsSync(item.tarball))
      throw new Error('Required package tarball is missing: ' + item.tarball);
    assertPackedManifest(item.manifest ?? readTarManifest(item.tarball), {
      name: item.name,
      version,
      kind: item.kind,
    });

    if (item.kind === 'node') {
      for (const platform of platformPackages) {
        if (
          !publishedThisRun.has(platform.name) &&
          !(await registryCheck(platform.name, version))
        ) {
          throw new Error(
            'Cannot publish ' +
              item.name +
              '@' +
              version +
              ': required platform package ' +
              platform.name +
              '@' +
              version +
              ' is not published.',
          );
        }
      }
    }
    if (await registryCheck(item.name, version)) {
      log(
        'Skipping already published ' +
          item.name +
          '@' +
          version +
          '; leaving its dist-tags unchanged.',
      );
      continue;
    }
    await publish(item.tarball, npmTag);
    publishedThisRun.add(item.name);
    log('Published ' + item.name + '@' + version + ' with npm dist-tag ' + npmTag + '.');
  }
}

async function publishTarball(tarball, npmTag) {
  execFileSync('npm', ['publish', tarball, '--access', 'public', '--tag', npmTag, '--provenance'], {
    cwd: rootDir,
    stdio: 'inherit',
  });
}

function writeGitHubOutput(values) {
  const outputPath = process.env.GITHUB_OUTPUT;
  const output = Object.entries(values)
    .map(([key, value]) => key + '=' + value)
    .join('\n');
  if (outputPath) writeFileSync(outputPath, output + '\n', { flag: 'a' });
  else process.stdout.write(output + '\n');
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  if (command === 'authorize') return authorizeRelease();
  if (command === 'detect') {
    const subject = execFileSync(
      'git',
      ['log', '-1', '--format=%s', process.env.GITHUB_SHA ?? 'HEAD'],
      { cwd: rootDir, encoding: 'utf8' },
    ).replace(/\r?\n$/, '');
    const release = detectRelease({
      subject,
      eventName: process.env.GITHUB_EVENT_NAME,
      ref: process.env.GITHUB_REF,
    });
    if (!release) return writeGitHubOutput({ is_release: 'false' });
    await authorizeRelease();
    return writeGitHubOutput({
      is_release: 'true',
      version: release.version,
      npm_tag: release.npmTag,
      prerelease: String(release.prerelease),
    });
  }
  if (command === 'sync') {
    const version = args[0] ?? process.env.RELEASE_VERSION;
    if (!version) throw new Error('Usage: release.mjs sync <version>');
    return syncWorkspaceVersions(version);
  }
  if (command === 'pack') {
    const [version, bindingsDirectory, serversDirectory, outputDirectory] = args;
    if (!outputDirectory) {
      throw new Error(
        'Usage: release.mjs pack <version> <bindings-dir> <servers-dir> <output-dir>',
      );
    }
    const packageList = packRelease({
      version,
      bindingsDirectory,
      serversDirectory,
      outputDirectory,
    });
    process.stdout.write('package_list=' + packageList + '\n');
    return;
  }
  if (command === 'publish') {
    const [version, npmTag, packageListPath] = args;
    if (!packageListPath) {
      throw new Error('Usage: release.mjs publish <version> <npm-tag> <package-list>');
    }
    const packageDirectory = dirname(resolve(packageListPath));
    const packages = JSON.parse(readFileSync(packageListPath, 'utf8')).map((item) => ({
      ...item,
      tarball: resolve(packageDirectory, item.tarball),
    }));
    return publishRelease({ packages, version, npmTag });
  }
  throw new Error('Commands: detect, authorize, sync, pack, publish');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error('Release pipeline error: ' + error.message);
    process.exitCode = 1;
  });
}
