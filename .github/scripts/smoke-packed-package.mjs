// Install the packed @remedia/node and its platform package into a clean project and use the
// native engine from a fresh Node.js process with every FFmpeg loader variable unset.
//
// This exercises the layout npm users receive: the engine addon and the FFmpeg-free bootstrap
// live only in `@remedia/node-<platform>`; nothing sits next to native.js.
//
// Usage: smoke-packed-package.mjs <rust-target-triple | platform-suffix>
//   REMEDIA_TEST_HOME           home directory containing the installer-managed ~/.remedia/ffmpeg9
//   REMEDIA_EXPECT_FFMPEG_SOURCE  optional expected runtime source (managed, system, explicit)
import { execFileSync, spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { RELEASE_TARGETS, bootstrapFileName } from './release.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const nodePackage = join(root, 'packages/remedia-node');
const fixture = join(root, 'crates/remedia-core/tests/fixtures/generated-tone.mp3');

const requested = process.argv[2];
const target = RELEASE_TARGETS.find(
  (candidate) => candidate.triple === requested || candidate.suffix === requested,
);

if (!target)
  throw new Error('Usage: smoke-packed-package.mjs <rust-target-triple | platform-suffix>');

const suffix = target.suffix;

const engine = `node.${suffix}.node`;
const bootstrap = bootstrapFileName(engine);

for (const file of [engine, bootstrap]) {
  if (!existsSync(join(nodePackage, file))) throw new Error(`Missing built artifact: ${file}`);
}

const home = process.env.REMEDIA_TEST_HOME;

if (!home)
  throw new Error('REMEDIA_TEST_HOME must point at the home directory with ~/.remedia/ffmpeg9');

// The path contains spaces on purpose; quoting mistakes show up as failures here.
const work = mkdtempSync(join(tmpdir(), 'remedia smoke project '));
const tarballs = join(work, 'tarballs');
const project = join(work, 'app with spaces');

try {
  mkdirSync(tarballs, { recursive: true });

  const pack = (directory) => {
    const before = new Set(readdirSync(tarballs));

    execFileSync('pnpm', ['pack', '--pack-destination', tarballs], {
      cwd: directory,
      stdio: 'inherit',
      shell: process.platform === 'win32',
    });

    const created = readdirSync(tarballs).filter((entry) => !before.has(entry));

    if (created.length !== 1) throw new Error(`Expected one tarball from ${directory}`);

    return join(tarballs, created[0]);
  };

  // Mirror the release: the root package holds JavaScript only; the platform package holds both
  // native files. Stage the platform package outside the workspace so pnpm treats it standalone.
  const platformDirectory = join(work, 'platform package');
  const version = JSON.parse(readFileSync(join(nodePackage, 'package.json'), 'utf8')).version;

  mkdirSync(platformDirectory, { recursive: true });
  writeFileSync(
    join(platformDirectory, 'package.json'),
    JSON.stringify({
      name: `@remedia/node-${suffix}`,
      version,
      main: engine,
      files: [engine, bootstrap],
    }),
  );

  for (const file of [engine, bootstrap]) {
    writeFileSync(join(platformDirectory, file), readFileSync(join(nodePackage, file)));
  }

  const rootTarball = pack(nodePackage);
  const platformTarball = pack(platformDirectory);

  const extract = (tarball, destination) => {
    mkdirSync(destination, { recursive: true });
    execFileSync('tar', ['-xzf', tarball, '--strip-components=1', '-C', destination]);
  };

  extract(rootTarball, join(project, 'node_modules', '@remedia', 'node'));
  extract(platformTarball, join(project, 'node_modules', `@remedia/node-${suffix}`));

  const installedRoot = join(project, 'node_modules', '@remedia', 'node');

  if (readdirSync(installedRoot).some((entry) => entry.endsWith('.node'))) {
    throw new Error(
      'The root package must not contain native addons; they belong to the platform package.',
    );
  }

  writeFileSync(
    join(project, 'package.json'),
    JSON.stringify({ name: 'smoke', type: 'module', private: true }),
  );
  writeFileSync(
    join(project, 'main.mjs'),
    `
import { Remedia, getFfmpegRuntime } from '@remedia/node';

const runtime = getFfmpegRuntime();
const remedia = await Remedia.create();
const info = await remedia.probe({ file: process.argv[2] });
const playback = remedia.createPlaybackSession();

await playback.play({ file: process.argv[2] });
await playback.wait();
await playback.dispose();
await remedia.close();

console.log(JSON.stringify({
  source: runtime.source,
  directory: runtime.directory,
  libraries: runtime.libraries.map((library) => library.name + ' ' + library.version),
  codec: info.codec,
  loaderVariables: Object.keys(process.env).filter((key) => /^(LD_|DYLD_|REMEDIA_FFMPEG)/.test(key)),
}));
`,
  );

  const windowsRoot = process.env.SystemRoot ?? 'C:\\Windows';
  const environment =
    process.platform === 'win32'
      ? {
          SystemRoot: windowsRoot,
          windir: windowsRoot,
          PATH: `${windowsRoot}\\System32;${windowsRoot}`,
          USERPROFILE: home,
          TEMP: tmpdir(),
          TMP: tmpdir(),
        }
      : { HOME: home, PATH: '/usr/bin:/bin', TMPDIR: tmpdir() };

  const result = spawnSync(process.execPath, [join(project, 'main.mjs'), fixture], {
    cwd: project,
    env: environment,
    encoding: 'utf8',
    timeout: 120_000,
  });

  process.stdout.write(result.stdout ?? '');
  process.stderr.write(result.stderr ?? '');

  if (result.status !== 0)
    throw new Error(`Packed package failed to start (exit ${result.status})`);

  const report = JSON.parse(result.stdout.trim().split('\n').at(-1));

  if (report.loaderVariables.length > 0) throw new Error('The child had loader variables set');
  if (report.libraries.length !== 5) throw new Error('Expected five loaded FFmpeg libraries');
  if (report.codec !== 'mp3') throw new Error(`Decoded unexpected codec ${report.codec}`);

  const expected = process.env.REMEDIA_EXPECT_FFMPEG_SOURCE;

  if (expected && report.source !== expected) {
    throw new Error(`Expected FFmpeg source ${expected}, got ${report.source}`);
  }

  console.log(
    `Packed @remedia/node (${suffix}) loaded FFmpeg from ${report.directory} (${report.source}).`,
  );
} finally {
  rmSync(work, { recursive: true, force: true });
}
