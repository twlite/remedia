// Install the packed Remedia packages into clean projects and use them from fresh Node.js processes
// with every FFmpeg loader variable unset.
//
// The engine lives only in `@remedia/node-<platform>`; nothing sits next to native.js. Every
// package ships one synchronous ESM build, so the same files must load through `import` and
// `require()` (Node.js require(esm)) with one module instance per process.
//
// Projects:
// - full: @remedia/node, its platform package, @remedia/client, @remedia/plugin-host, @remedia/config.
// - remote-only: @remedia/client, @remedia/plugin-host, @remedia/config without @remedia/node.
// - missing-binding: @remedia/node without its platform package.
//
// Usage: smoke-packed-package.mjs <rust-target-triple | platform-suffix>
import { execFileSync, spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { RELEASE_TARGETS, stageReleaseLicenses } from './release.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const packages = {
  node: join(root, 'packages/remedia-node'),
  client: join(root, 'packages/remedia-client'),
  pluginHost: join(root, 'packages/remedia-plugin-host'),
  config: join(root, 'packages/remedia-config'),
};
const fixture = join(root, 'crates/remedia-core/tests/fixtures/generated-tone.mp3');

const requested = process.argv[2];
const target = RELEASE_TARGETS.find(
  (candidate) => candidate.triple === requested || candidate.suffix === requested,
);

if (!target)
  throw new Error('Usage: smoke-packed-package.mjs <rust-target-triple | platform-suffix>');

const suffix = target.suffix;

const engine = `node.${suffix}.node`;

if (!existsSync(join(packages.node, engine))) throw new Error(`Missing built artifact: ${engine}`);

const windows = process.platform === 'win32';

// Run pnpm without a shell. pnpm uses a cmd shim on Windows, so pass arguments as data through
// PowerShell rather than concatenating unquoted arguments with shell: true.
function pnpm(directory, args, env = {}) {
  const command = windows ? 'pwsh' : 'pnpm';
  const argv = windows
    ? ['-NoProfile', '-NonInteractive', '-Command', `pnpm ${args.join(' ')}; exit $LASTEXITCODE`]
    : args;

  execFileSync(command, argv, {
    cwd: directory,
    stdio: 'inherit',
    env: { ...process.env, ...env },
  });
}

// The published files are compiled TypeScript; build them from the current sources.
for (const directory of [packages.config, packages.pluginHost, packages.client]) {
  pnpm(directory, ['run', 'build']);
}

pnpm(packages.node, ['run', 'build:types']);

const home = mkdtempSync(join(tmpdir(), 'remedia empty home '));

// The path contains spaces on purpose; quoting mistakes show up as failures here.
const work = mkdtempSync(join(tmpdir(), 'remedia smoke project '));
const tarballs = join(work, 'tarballs');

try {
  mkdirSync(tarballs, { recursive: true });

  const pack = (directory) => {
    const before = new Set(readdirSync(tarballs));

    if (windows) {
      pnpm(directory, ['pack', '--pack-destination', '$env:REMEDIA_PACK_DESTINATION'], {
        REMEDIA_PACK_DESTINATION: tarballs,
      });
    } else {
      pnpm(directory, ['pack', '--pack-destination', tarballs]);
    }

    const created = readdirSync(tarballs).filter((entry) => !before.has(entry));

    if (created.length !== 1) throw new Error(`Expected one tarball from ${directory}`);

    return join(tarballs, created[0]);
  };

  // Mirror the release: the root package holds JavaScript only; the platform package holds the
  // native file. Stage the platform package outside the workspace so pnpm treats it standalone.
  const platformDirectory = join(work, 'platform package');
  const notices = [
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'THIRD_PARTY_LICENSES.txt',
    'LICENSES.md',
    'COPYING.GPLv3',
    'SOURCE_DISTRIBUTION.md',
  ];
  const version = JSON.parse(readFileSync(join(packages.node, 'package.json'), 'utf8')).version;

  mkdirSync(platformDirectory, { recursive: true });
  writeFileSync(
    join(platformDirectory, 'package.json'),
    JSON.stringify({
      name: `@remedia/node-${suffix}`,
      version,
      main: engine,
      license: 'SEE LICENSE IN LICENSES.md',
      files: [engine, ...notices],
    }),
  );
  writeFileSync(join(platformDirectory, engine), readFileSync(join(packages.node, engine)));
  stageReleaseLicenses(platformDirectory);

  const tarball = {
    node: pack(packages.node),
    platform: pack(platformDirectory),
    client: pack(packages.client),
    pluginHost: pack(packages.pluginHost),
    config: pack(packages.config),
  };

  const packageNames = {
    node: '@remedia/node',
    platform: `@remedia/node-${suffix}`,
    client: '@remedia/client',
    pluginHost: '@remedia/plugin-host',
    config: '@remedia/config',
  };

  // Third-party runtime dependencies come from the workspace install, so this check stays offline.
  const thirdParty = {
    ws: dirname(createRequire(join(packages.client, 'package.json')).resolve('ws/package.json')),
    zod: dirname(createRequire(join(packages.config, 'package.json')).resolve('zod/package.json')),
  };

  const createProject = (name, members) => {
    const project = join(work, name);

    for (const member of members) {
      const destination = join(project, 'node_modules', ...packageNames[member].split('/'));

      mkdirSync(destination, { recursive: true });
      execFileSync('tar', ['-xzf', tarball[member], '--strip-components=1', '-C', destination]);
    }

    for (const [dependency, source] of Object.entries(thirdParty)) {
      // A junction needs no elevated rights on Windows; POSIX ignores the type.
      symlinkSync(source, join(project, 'node_modules', dependency), 'junction');
    }

    // No "type" field: `.js` would be CommonJS. Scripts use explicit `.cjs` and `.mjs` extensions.
    writeFileSync(join(project, 'package.json'), JSON.stringify({ name, private: true }));

    return project;
  };

  const full = createProject('app with spaces', [
    'node',
    'platform',
    'client',
    'pluginHost',
    'config',
  ]);
  const remoteOnly = createProject('remote only app', ['client', 'pluginHost', 'config']);
  const missingBinding = createProject('missing binding app', ['node']);

  if (
    readdirSync(join(full, 'node_modules', '@remedia', 'node')).some((entry) =>
      entry.endsWith('.node'),
    )
  ) {
    throw new Error(
      'The root package must not contain native addons; they belong to the platform package.',
    );
  }

  const windowsRoot = process.env.SystemRoot ?? 'C:\\Windows';
  const environment = windows
    ? {
        SystemRoot: windowsRoot,
        windir: windowsRoot,
        PATH: `${windowsRoot}\\System32;${windowsRoot}`,
        USERPROFILE: home,
        TEMP: tmpdir(),
        TMP: tmpdir(),
      }
    : { HOME: home, PATH: '/usr/bin:/bin', TMPDIR: tmpdir() };

  // Run one script in a fresh process and return its last stdout line parsed as JSON.
  const run = (project, file, source, args = []) => {
    writeFileSync(join(project, file), source);

    // Fail on any warning, such as an ExperimentalWarning for require(esm).
    const result = spawnSync(
      process.execPath,
      ['--throw-deprecation', join(project, file), ...args],
      { cwd: project, env: environment, encoding: 'utf8', timeout: 120_000 },
    );

    process.stdout.write(result.stdout ?? '');
    process.stderr.write(result.stderr ?? '');

    if (result.status !== 0)
      throw new Error(`${file} in ${project} failed (exit ${result.status ?? result.signal})`);

    if (/warning/i.test(result.stderr ?? ''))
      throw new Error(`${file} in ${project} emitted a warning`);

    return JSON.parse(result.stdout.trim().split('\n').at(-1));
  };

  // Native addons appear in Module._cache because the napi-rs loader uses createRequire.
  const nativeCount = `Object.keys(nativeCache).filter((key) => key.endsWith('.node')).length`;

  const playback = (load) => `
${load}
const fixture = process.argv[2];
const runtime = getFFmpegRuntime();
const remedia = await Remedia.create();
const info = await remedia.probe({ file: fixture });
const playback = remedia.createPlaybackSession();

await playback.play({ file: fixture });
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
`;

  const checkPlayback = (report, style) => {
    if (report.loaderVariables.length > 0) throw new Error('The child had loader variables set');
    if (report.libraries.length !== 5) throw new Error('Expected five loaded FFmpeg libraries');
    if (report.codec !== 'mp3') throw new Error(`Decoded unexpected codec ${report.codec}`);
    if (report.source !== 'bundled')
      throw new Error(`Expected FFmpeg source bundled, got ${report.source}`);

    console.log(`${style}: packed @remedia/node (${suffix}) played with ${report.source} FFmpeg.`);
  };

  // 1. Native playback through both import styles.
  checkPlayback(
    run(
      full,
      'playback.mjs',
      playback(`import { Remedia, getFFmpegRuntime } from '@remedia/node';`),
      [fixture],
    ),
    'ESM',
  );
  checkPlayback(
    run(
      full,
      'playback.cjs',
      `(async () => {${playback(`const { Remedia, getFFmpegRuntime } = require('@remedia/node');`)}})().catch((error) => { console.error(error); process.exitCode = 1; });`,
      [fixture],
    ),
    'CommonJS',
  );

  // 2. Every entry point and subpath through both import styles.
  const entryPoints = `
if (typeof RemediaClient.create !== 'function') throw new Error('RemediaClient');
if (typeof Remedia.create !== 'function') throw new Error('Remedia');
if (typeof getFFmpegRuntime().version !== 'string') throw new Error('getFFmpegRuntime');
const host = createPluginHost({ name: 'interop' });
if (typeof createNodeMiddleware(host) !== 'function') throw new Error('createNodeMiddleware');
await host.close();
if (RemediaConfigSchema.parse({ auth: { password: 'secret' } }).auth.password !== 'secret')
  throw new Error('RemediaConfigSchema');
if (schema.$schema !== 'https://json-schema.org/draft/2020-12/schema') throw new Error('schema.json');
`;

  run(
    full,
    'entry-points.mjs',
    `
import { RemediaClient } from '@remedia/client';
import { Remedia, getFFmpegRuntime } from '@remedia/node';
import { createPluginHost } from '@remedia/plugin-host';
import { createNodeMiddleware } from '@remedia/plugin-host/node';
import { RemediaConfigSchema } from '@remedia/config';
import schema from '@remedia/config/schema.json' with { type: 'json' };
${entryPoints}
console.log('{}');
`,
  );
  run(
    full,
    'entry-points.cjs',
    `
const { RemediaClient } = require('@remedia/client');
const { Remedia, getFFmpegRuntime } = require('@remedia/node');
const { createPluginHost } = require('@remedia/plugin-host');
const { createNodeMiddleware } = require('@remedia/plugin-host/node');
const { RemediaConfigSchema } = require('@remedia/config');
const schema = require('@remedia/config/schema.json');
(async () => {
${entryPoints}
console.log('{}');
})().catch((error) => { console.error(error); process.exitCode = 1; });
`,
  );
  console.log('ESM and CommonJS: every package entry point and subpath export loaded.');

  // 3. Mixed require() and import() share one module instance and one addon.
  const mixed = (header) => `
${header}
const required = {
  node: require('@remedia/node'),
  client: require('@remedia/client'),
  pluginHost: require('@remedia/plugin-host'),
  config: require('@remedia/config'),
};
const imported = {
  node: await import('@remedia/node'),
  client: await import('@remedia/client'),
  pluginHost: await import('@remedia/plugin-host'),
  config: await import('@remedia/config'),
};
const same =
  required.node.Remedia === imported.node.Remedia &&
  required.node.getFFmpegRuntime === imported.node.getFFmpegRuntime &&
  required.client.RemediaClient === imported.client.RemediaClient &&
  required.client.RemediaClientError === imported.client.RemediaClientError &&
  required.pluginHost.PluginHost === imported.pluginHost.PluginHost &&
  required.config.RemediaConfigSchema === imported.config.RemediaConfigSchema;
const host = required.pluginHost.createPluginHost({ name: 'mixed' });
const crossInstance = host instanceof imported.pluginHost.PluginHost;
await host.close();
console.log(JSON.stringify({ same, crossInstance, addons: ${nativeCount} }));
`;

  for (const [file, source] of [
    [
      'mixed.cjs',
      `(async () => {${mixed('const nativeCache = require.cache;')}})().catch((error) => { console.error(error); process.exitCode = 1; });`,
    ],
    [
      'mixed.mjs',
      mixed(
        `import { createRequire } from 'node:module';\nconst require = createRequire(import.meta.url);\nconst nativeCache = require.cache;`,
      ),
    ],
  ]) {
    const report = run(full, file, source);

    if (!report.same || !report.crossInstance)
      throw new Error(`${file}: require() and import() returned different module instances`);
    if (report.addons !== 1)
      throw new Error(`${file}: expected one loaded addon, got ${report.addons}`);
  }
  console.log('Mixed require() and import(): one module instance and one addon per process.');

  // 4. The client loads the addon only when an embedded node starts; loading packages starts nothing.
  const lazy = (header) => `
${header}
const before = ${nativeCount};
const remedia = await RemediaClient.create({ nodes: [{ type: 'embedded', id: 'local' }], send() {} });
const after = ${nativeCount};
const status = remedia.nodes[0].status;
await remedia.close();
console.log(JSON.stringify({ before, after, status }));
`;

  for (const [file, source] of [
    [
      'embedded.cjs',
      `const { RemediaClient } = require('@remedia/client');\n(async () => {${lazy('const nativeCache = require.cache;')}})().catch((error) => { console.error(error); process.exitCode = 1; });`,
    ],
    [
      'embedded.mjs',
      lazy(
        `import { createRequire } from 'node:module';\nimport { RemediaClient } from '@remedia/client';\nconst nativeCache = createRequire(import.meta.url).cache;`,
      ),
    ],
  ]) {
    const report = run(full, file, source);

    if (report.before !== 0) throw new Error(`${file}: loading @remedia/client loaded the addon`);
    if (report.after !== 1) throw new Error(`${file}: expected one addon after start`);
    if (report.status !== 'healthy')
      throw new Error(`${file}: embedded node reported ${report.status}`);
  }
  console.log('Embedded client: addon loaded once, only by RemediaClient.create().');

  // Loading every package must not keep the event loop alive or start audio resources.
  const idle = run(
    full,
    'idle.cjs',
    `
require('@remedia/node');
require('@remedia/client');
require('@remedia/plugin-host');
require('@remedia/plugin-host/node');
require('@remedia/config');
console.log(JSON.stringify({ resources: process.getActiveResourcesInfo() }));
`,
  );

  if (idle.resources.length > 0)
    throw new Error(`Loading the packages started resources: ${idle.resources.join(', ')}`);
  console.log('Loading the packages starts no resources and the process exits on its own.');

  // 5. Remote-only applications never need @remedia/node.
  const remote = `
const closed = 'ws://127.0.0.1:9';
const client = await RemediaClient.create({
  botUserId: '1',
  nodes: [{ type: 'remote', id: 'remote', url: closed, auth: { username: 'user', password: 'pass' } }],
  send() {},
});
const remoteStatus = client.nodes[0].status;
await client.close();
const embedded = await RemediaClient.create({ nodes: [{ type: 'embedded', id: 'local' }], send() {} });
const embeddedNode = embedded.nodes[0];
await embedded.close();
const host = createPluginHost({ name: 'remote' });
await host.close();
RemediaConfigSchema.parse({ auth: { password: 'secret' } });
console.log(JSON.stringify({ remoteStatus, embeddedStatus: embeddedNode.status, embeddedError: embeddedNode.lastError }));
`;

  for (const [file, source] of [
    [
      'remote.cjs',
      `const { RemediaClient } = require('@remedia/client');\nconst { createPluginHost } = require('@remedia/plugin-host');\nconst { RemediaConfigSchema } = require('@remedia/config');\n(async () => {${remote}})().catch((error) => { console.error(error); process.exitCode = 1; });`,
    ],
    [
      'remote.mjs',
      `import { RemediaClient } from '@remedia/client';\nimport { createPluginHost } from '@remedia/plugin-host';\nimport { RemediaConfigSchema } from '@remedia/config';\n${remote}`,
    ],
  ]) {
    const report = run(remoteOnly, file, source);

    if (report.remoteStatus === 'healthy') throw new Error(`${file}: closed port reported healthy`);
    if (report.embeddedStatus !== 'disconnected')
      throw new Error(
        `${file}: embedded node without @remedia/node reported ${report.embeddedStatus}`,
      );
    if (!/embedded @remedia\/node could not be loaded: .*@remedia\/node/.test(report.embeddedError))
      throw new Error(`${file}: uninformative embedded error: ${report.embeddedError}`);
  }
  console.log('Remote-only client: loads and starts without @remedia/node installed.');

  // 6. A missing platform package keeps the napi-rs loader's diagnostics.
  const missing = (load) => `
${load}
  console.log(JSON.stringify({ loaded: true }));
} catch (error) {
  const causes = [];
  for (let cause = error.cause; cause; cause = cause.cause) causes.push(String(cause.message));
  console.log(JSON.stringify({ message: error.message, causes }));
}
`;

  for (const [file, source] of [
    ['missing.cjs', missing(`try {\n  require('@remedia/node');`)],
    ['missing.mjs', missing(`try {\n  await import('@remedia/node');`)],
  ]) {
    const report = run(missingBinding, file, source);

    if (report.loaded) throw new Error(`${file}: loaded without a native binding`);
    if (!report.message.startsWith('Cannot find native binding'))
      throw new Error(`${file}: unexpected error ${report.message}`);
    if (!report.causes.some((cause) => cause.includes(`@remedia/node-${suffix}`)))
      throw new Error(`${file}: load error does not name @remedia/node-${suffix}`);
  }
  console.log('Missing platform package: both import styles report the napi-rs load chain.');

  console.log(`Packed packages passed on Node.js ${process.version} (${suffix}).`);
} finally {
  rmSync(work, { recursive: true, force: true });
  rmSync(home, { recursive: true, force: true });
}
