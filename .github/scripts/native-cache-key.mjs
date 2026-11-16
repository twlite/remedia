// Include actual compiler/tool versions as well as every native recipe and policy input.
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { readFileSync, appendFileSync } from 'node:fs';

const hash = createHash('sha256');
const files = execFileSync(
  'git',
  [
    'ls-files',
    '.github/native-deps',
    '.github/scripts/build-ffmpeg*',
    '.github/scripts/install-windows*',
    '.github/scripts/audit-ffmpeg*',
    '.github/scripts/native-cache-key.mjs',
    'rust-toolchain.toml',
  ],
  { encoding: 'utf8' },
)
  .trim()
  .split('\n');
for (const file of files) hash.update(file).update(readFileSync(file));
// New files are not in the index while developing; the workflow always has them checked in.
hash.update(readFileSync(new URL('./native-cache-key.mjs', import.meta.url)));
for (const [tool, args] of [
  ['rustc', ['-vV']],
  [process.env.CC || 'cc', ['--version']],
  ...(process.env.RUST_TARGET === 'armv7-unknown-linux-gnueabihf'
    ? [['arm-linux-gnueabihf-gcc', ['--version']]]
    : []),
  ['cl', ['/Bv']],
  ['nasm', ['-v']],
  ['zig', ['version']],
  ['cmake', ['--version']],
  ...(process.platform === 'darwin' ? [['xcrun', ['--sdk', 'macosx', '--show-sdk-version']]] : []),
]) {
  const result = spawnSync(tool, args, { encoding: 'utf8' });
  hash
    .update(tool)
    .update(result.stdout ?? '')
    .update(result.stderr ?? '')
    .update(String(result.status));
}
for (const key of [
  'RUST_TARGET',
  'RUNNER_OS',
  'RUNNER_ARCH',
  'VCToolsVersion',
  'WindowsSDKVersion',
  'ANDROID_NDK_HOME',
  'CC',
  'HOST_CC',
  'CFLAGS',
  'CPPFLAGS',
  'CXXFLAGS',
  'LDFLAGS',
  'MACOSX_DEPLOYMENT_TARGET',
  'SDKROOT',
])
  hash.update(key + '=' + (process.env[key] ?? ''));
const key = `native-v2-${process.env.RUST_TARGET}-${hash.digest('hex')}`;
if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `key=${key}\n`);
console.log(key);
