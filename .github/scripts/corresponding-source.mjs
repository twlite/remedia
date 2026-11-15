import { execFileSync } from 'node:child_process';
import { cpSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { RELEASE_TARGETS } from './release.mjs';
import { createHash } from 'node:crypto';

const destination = resolve(process.argv[2]);
const staging = resolve(process.argv[3]);
mkdirSync(staging, { recursive: true });
execFileSync('git', ['archive', '--format=tar', '-o', join(staging, 'remedia.tar'), 'HEAD']);
writeFileSync(join(staging, 'release.patch'), execFileSync('git', ['diff', '--binary', 'HEAD']));
const config = execFileSync('cargo', ['vendor', '--locked', join(staging, 'vendor')], {
  encoding: 'utf8',
  stdio: ['ignore', 'pipe', 'inherit'],
});
writeFileSync(join(staging, 'vendor-config.toml'), config);
writeFileSync(
  join(staging, 'BUILD.txt'),
  `Remedia revision: ${execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()}
Extract remedia.tar, apply release.patch, and merge vendor-config.toml into .cargo/config.toml.
Target source bundles contain native dependency source and exact generated configurations.
Use .github/workflows/platform.yaml and the pinned toolchain/manifests to reconstruct each target.
FFmpeg sources are unmodified unless patches are present in their target bundle.
`,
);
const targetSources = readdirSync(process.env.REMEDIA_TARGET_SOURCES);
for (const { triple } of RELEASE_TARGETS) {
  const name = `native-source-${triple}.tar.gz`;
  if (!targetSources.includes(name)) throw new Error(`Missing corresponding source: ${name}`);
  execFileSync('tar', ['-tzf', join(process.env.REMEDIA_TARGET_SOURCES, name)], {
    stdio: 'ignore',
  });
}
execFileSync(process.execPath, [
  '.github/scripts/audit-linked-licenses.mjs',
  join(staging, 'linked-license-inventory.json'),
]);
cpSync(process.env.REMEDIA_TARGET_SOURCES, join(staging, 'targets'), { recursive: true });
const hashes = [];
function walk(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) walk(path);
    else if (entry.isFile())
      hashes.push(
        `${createHash('sha256').update(readFileSync(path)).digest('hex')}  ${path.slice(staging.length + 1)}`,
      );
  }
}
walk(staging);
writeFileSync(join(staging, 'SHA256SUMS'), hashes.sort().join('\n') + '\n');
execFileSync('tar', ['-czf', destination, '-C', staging, '.']);
