// Rewrite the FFmpeg dependencies of a macOS N-API addon to `@rpath/<library>.dylib`.
//
// A linker records the install name of the dylib it linked against. Homebrew's FFmpeg uses an
// absolute install name (/opt/homebrew/opt/ffmpeg/lib/libavcodec.63.dylib), and dyld will not
// substitute a different file for an absolute dependency, so preloading Remedia's managed FFmpeg
// (whose install names are @rpath/...) would leave two copies of FFmpeg mapped in the process.
// With @rpath names, dyld reuses an already-loaded image of the same install name, which lets
// @remedia/node's bootstrap select the installation. The fallback rpaths below keep a plain
// Homebrew installation working when nothing was preloaded.
//
// install_name_tool invalidates the code signature; arm64 macOS refuses to load unsigned code,
// so the addon is ad-hoc signed again afterwards.
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { ffmpegAbi } from './ffmpeg-abi.mjs';

/** Homebrew prefixes for Apple Silicon and Intel; resolved by dyld only if nothing is preloaded. */
export const FALLBACK_RPATHS = ['/opt/homebrew/opt/ffmpeg/lib', '/usr/local/opt/ffmpeg/lib'];

const ffmpegDylib = new RegExp(
  `^lib(?:${[...ffmpegAbi.keys()].map((name) => name.slice(3)).join('|')})\\.\\d+\\.dylib$`,
);

/** Dependency paths from `otool -L` output, excluding the file's own header line. */
export function parseDependencies(otoolOutput) {
  return otoolOutput
    .split('\n')
    .slice(1)
    .map((line) => /^\s+(.+?) \(compatibility version/.exec(line)?.[1])
    .filter(Boolean);
}

/** LC_RPATH entries from `otool -l` output. */
export function parseRpaths(otoolOutput) {
  const rpaths = [];
  const lines = otoolOutput.split('\n');

  for (let index = 0; index < lines.length; index += 1) {
    if (!/^\s+cmd LC_RPATH\s*$/.test(lines[index])) continue;

    for (let offset = 1; offset <= 3; offset += 1) {
      const match = /^\s+path (.+?) \(offset \d+\)/.exec(lines[index + offset] ?? '');

      if (match) {
        rpaths.push(match[1]);
        break;
      }
    }
  }

  return rpaths;
}

/** Whether a dependency path names one of the FFmpeg libraries and is not yet @rpath-relative. */
export function needsRewrite(dependency) {
  return !dependency.startsWith('@rpath/') && ffmpegDylib.test(dependency.split('/').at(-1));
}

/** Normalize one addon in place and return what changed. */
export function normalizeMacosAddon(file, tools = { run: runTool }) {
  const dependencies = parseDependencies(tools.run('otool', ['-L', file]));
  const rewritten = [];

  for (const dependency of dependencies.filter(needsRewrite)) {
    const replacement = `@rpath/${dependency.split('/').at(-1)}`;

    tools.run('install_name_tool', ['-change', dependency, replacement, file]);
    rewritten.push([dependency, replacement]);
  }

  const existing = parseRpaths(tools.run('otool', ['-l', file]));
  const added = FALLBACK_RPATHS.filter((rpath) => !existing.includes(rpath));

  for (const rpath of added) tools.run('install_name_tool', ['-add_rpath', rpath, file]);

  if (rewritten.length > 0 || added.length > 0) {
    tools.run('codesign', ['--force', '--sign', '-', file]);
  }

  const final = parseDependencies(tools.run('otool', ['-L', file]));
  const missing = [...ffmpegAbi.keys()].filter(
    (name) => !final.some((dependency) => dependency.startsWith(`@rpath/${name}.`)),
  );

  if (missing.length > 0) {
    throw new Error(`${file} does not depend on @rpath FFmpeg libraries: ${missing.join(', ')}`);
  }

  return { file, rewritten, addedRpaths: added };
}

function runTool(command, args) {
  return execFileSync(command, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

function main(argv) {
  if (process.platform !== 'darwin') {
    console.log('Skipping macOS install-name normalization on a non-macOS host.');

    return;
  }

  const packageDirectory = resolve(
    dirname(fileURLToPath(import.meta.url)),
    '../../packages/remedia-node',
  );
  const files = argv.length
    ? argv
    : readdirSync(packageDirectory)
        .filter((entry) => /^node\.darwin-.+\.node$/.test(entry))
        .map((entry) => join(packageDirectory, entry));

  if (files.length === 0) {
    console.log('No macOS N-API addon found to normalize.');

    return;
  }

  for (const file of files) {
    if (!existsSync(file)) throw new Error(`Addon not found: ${file}`);

    const result = normalizeMacosAddon(file);

    console.log(
      `Normalized ${file}: ${result.rewritten.length} FFmpeg dependencies -> @rpath, ` +
        `${result.addedRpaths.length} fallback rpaths added`,
    );
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2));
}
