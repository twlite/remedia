// Build @remedia/node's FFmpeg-free native bootstrap addon with the same napi-rs CLI, targets and
// cross-compilation switches as the engine addon. The result is `bootstrap.<platform>.node` in
// packages/remedia-node, next to `node.<platform>.node`.
//
// Usage: build-bootstrap.mjs [--target <triple>] [--cross-compile] [--debug]
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export function bootstrapBuildArguments(argv) {
  const args = [
    'exec',
    'napi',
    'build',
    '--platform',
    '--no-js',
    '--manifest-path',
    'bootstrap/Cargo.toml',
    '--config-path',
    'bootstrap/napi.json',
    '--package-json-path',
    'package.json',
    '--output-dir',
    '.',
    // The typedef is unused: lib/bootstrap.ts declares the two exported functions by hand.
    '--dts',
    '../../target/remedia-node-bootstrap.d.ts',
  ];

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];

    if (argument === '--target') {
      const target = argv[(index += 1)];

      if (!target) throw new Error('--target requires a Rust target triple');

      args.push('--target', target);
    } else if (argument === '--cross-compile') {
      args.push('--cross-compile');
    } else if (argument === '--debug') {
      // Debug builds are the napi-rs default; --release is added below otherwise.
    } else {
      throw new Error(`Unsupported argument: ${argument}`);
    }
  }

  if (!argv.includes('--debug')) args.push('--release');

  return args;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const packageDirectory = resolve(
    dirname(fileURLToPath(import.meta.url)),
    '../../packages/remedia-node',
  );

  execFileSync('pnpm', bootstrapBuildArguments(process.argv.slice(2)), {
    cwd: packageDirectory,
    stdio: 'inherit',
    shell: process.platform === 'win32',
  });
}
