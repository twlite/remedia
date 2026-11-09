import { readdirSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { ffmpegAbi } from './ffmpeg-abi.mjs';

const requiredLinuxLibraries = [...ffmpegAbi].map(([library, major]) => `${library}.so.${major}`);
const requiredMacLibraries = [...ffmpegAbi].map(([library, major]) => `${library}.${major}`);
const requiredWindowsLibraries = [...ffmpegAbi].map(
  ([library, major]) => `${library.slice(3)}-${major}.dll`,
);

const packageDirectory = 'packages/remedia-node';
const addons = readdirSync(packageDirectory).filter((entry) => /^node\..+\.node$/.test(entry));
if (addons.length !== 1) {
  throw new Error(
    `Expected exactly one target N-API addon, found: ${addons.join(', ') || '(none)'}`,
  );
}

// The bootstrap loads before FFmpeg and must therefore not link against it.
const bootstrapName = 'bootstrap.' + addons[0].slice('node.'.length);
if (!readdirSync(packageDirectory).includes(bootstrapName)) {
  throw new Error(`Missing bootstrap addon ${bootstrapName} next to ${addons[0]}`);
}
const bootstrapBinary = `${packageDirectory}/${bootstrapName}`;

const serverBinary = process.env.REMEDIA_SERVER_BINARY;
if (!serverBinary)
  throw new Error('REMEDIA_SERVER_BINARY must point to the downloaded server artifact');
const serverName = basename(serverBinary);
if (!serverName.startsWith('remedia-server-'))
  throw new Error(`Unexpected server artifact name: ${serverName}`);
const runtimeBinary = join(
  dirname(serverBinary),
  'remedia-server-runtime-' + serverName.slice('remedia-server-'.length),
);
const nativeBinaries = [`${packageDirectory}/${addons[0]}`, runtimeBinary];
const binaries = [serverBinary, ...nativeBinaries, bootstrapBinary];
const mustNotLinkFfmpeg = new Set([serverBinary, bootstrapBinary]);
console.log(
  `Testing native addon ${addons[0]}, bootstrap ${bootstrapName}, CLI ${serverName}, and runtime ${basename(runtimeBinary)}`,
);
const version = execFileSync(serverBinary, ['--version'], {
  encoding: 'utf8',
  env: { ...process.env, LD_LIBRARY_PATH: '', DYLD_LIBRARY_PATH: '' },
});
if (!version.includes('remedia-server'))
  throw new Error('The standalone CLI did not respond without FFmpeg loader paths');

if (process.platform === 'linux') {
  for (const binary of binaries) {
    const header = execFileSync('readelf', ['-h', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('ldd', [binary], { encoding: 'utf8' });
    console.log(`${binary}\n${dependencies}`);
    if (dependencies.includes('not found'))
      throw new Error(`Unresolved runtime dependency in ${binary}`);
    if (!header.includes(process.arch === 'arm64' ? 'AArch64' : 'Advanced Micro Devices X86-64')) {
      throw new Error(`Unexpected ELF architecture for ${binary}`);
    }
    if (mustNotLinkFfmpeg.has(binary)) {
      rejectDependencies(dependencies, requiredLinuxLibraries, binary);
    } else {
      requireDependencies(dependencies, requiredLinuxLibraries, binary);
    }
    const directDependencies = execFileSync('readelf', ['-d', binary], { encoding: 'utf8' });
    rejectDynamicOpus(directDependencies, binary, /libopus\.so(?:\.\d+)?/i);
  }
} else if (process.platform === 'darwin') {
  for (const binary of binaries) {
    const architecture = execFileSync('lipo', ['-info', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('otool', ['-L', binary], { encoding: 'utf8' });
    console.log(`${architecture}\n${dependencies}`);
    if (!architecture.includes(process.arch === 'arm64' ? 'arm64' : 'x86_64')) {
      throw new Error(`Unexpected Mach-O architecture for ${binary}`);
    }
    if (mustNotLinkFfmpeg.has(binary)) {
      rejectDependencies(dependencies, requiredMacLibraries, binary);
    } else {
      requireDependencies(dependencies, requiredMacLibraries, binary);
    }
    if (binary === nativeBinaries[0]) requireRpathInstallNames(dependencies, binary);
    rejectDynamicOpus(dependencies, binary, /libopus(?:\.\d+)?\.dylib/i);
  }
} else if (process.platform === 'win32') {
  for (const binary of binaries) {
    const headers = execFileSync('dumpbin', ['/headers', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('dumpbin', ['/dependents', binary], { encoding: 'utf8' });
    console.log(`${binary}\n${dependencies}`);
    if (!headers.toLowerCase().includes('machine (x64)')) {
      throw new Error(`Unexpected PE architecture for ${binary}`);
    }
    if (mustNotLinkFfmpeg.has(binary)) {
      rejectDependencies(dependencies, requiredWindowsLibraries, binary);
    } else {
      requireDependencies(dependencies, requiredWindowsLibraries, binary);
    }
    rejectDynamicOpus(dependencies, binary, /opus\.dll/i);
  }
} else {
  throw new Error(`Native artifact inspection is not configured for ${process.platform}`);
}

function requireDependencies(output, names, binary) {
  for (const name of names) {
    if (!output.toLowerCase().includes(name.toLowerCase())) {
      throw new Error(`${binary} does not declare the expected dynamic dependency ${name}`);
    }
  }
}

// dyld reuses an already-loaded image only when its install name equals the requested one. The
// engine addon must therefore request @rpath names so @remedia/node can choose the installation.
function requireRpathInstallNames(output, binary) {
  for (const [library, major] of ffmpegAbi) {
    if (!output.includes(`@rpath/${library}.${major}.dylib`)) {
      throw new Error(
        `${binary} must depend on @rpath/${library}.${major}.dylib; run normalize-macos-addon.mjs`,
      );
    }
  }
  const loadCommands = execFileSync('otool', ['-l', binary], { encoding: 'utf8' });
  for (const rpath of ['/opt/homebrew/opt/ffmpeg/lib', '/usr/local/opt/ffmpeg/lib']) {
    if (!loadCommands.includes(`path ${rpath} `)) {
      throw new Error(`${binary} is missing the fallback rpath ${rpath}`);
    }
  }
  execFileSync('codesign', ['--verify', '--strict', binary]);
}

function rejectDynamicOpus(output, binary, pattern) {
  if (pattern.test(output)) {
    throw new Error(
      `${binary} unexpectedly depends on dynamically linked libopus; the addon bundles it`,
    );
  }
}

function rejectDependencies(output, names, binary) {
  for (const name of names) {
    if (output.toLowerCase().includes(name.toLowerCase())) {
      throw new Error(
        `${binary} unexpectedly links to ${name}; it must not require FFmpeg to load`,
      );
    }
  }
}
