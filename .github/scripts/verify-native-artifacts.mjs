import { readdirSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { execFileSync } from 'node:child_process';
const packageDirectory = 'packages/remedia-node';
const addons = readdirSync(packageDirectory).filter((entry) => /^node\..+\.node$/.test(entry));
if (addons.length !== 1) {
  throw new Error(
    `Expected exactly one target N-API addon, found: ${addons.join(', ') || '(none)'}`,
  );
}

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
const binaries = [serverBinary, ...nativeBinaries];
console.log(
  `Testing native addon ${addons[0]}, CLI ${serverName}, and runtime ${basename(runtimeBinary)}`,
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
    rejectBundledDependencies(dependencies, binary);
  }
} else if (process.platform === 'darwin') {
  for (const binary of binaries) {
    const architecture = execFileSync('lipo', ['-info', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('otool', ['-L', binary], { encoding: 'utf8' });
    console.log(`${architecture}\n${dependencies}`);
    if (!architecture.includes(process.arch === 'arm64' ? 'arm64' : 'x86_64')) {
      throw new Error(`Unexpected Mach-O architecture for ${binary}`);
    }
    rejectBundledDependencies(dependencies, binary);
  }
} else if (process.platform === 'win32') {
  for (const binary of binaries) {
    const headers = execFileSync('dumpbin', ['/headers', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('dumpbin', ['/dependents', binary], { encoding: 'utf8' });
    console.log(`${binary}\n${dependencies}`);
    if (
      !headers
        .toLowerCase()
        .includes(process.arch === 'arm64' ? 'machine (arm64)' : 'machine (x64)')
    ) {
      throw new Error(`Unexpected PE architecture for ${binary}`);
    }
    rejectBundledDependencies(dependencies, binary);
  }
} else {
  throw new Error(`Native artifact inspection is not configured for ${process.platform}`);
}

function rejectBundledDependencies(output, binary) {
  if (
    /(?:lib)?(?:avcodec|avformat|avutil|avfilter|swresample|swscale|avdevice|opus|ssl|crypto)[-.\d]*(?:\.so|\.dylib|\.dll)/i.test(
      output,
    )
  ) {
    throw new Error(`${binary} unexpectedly depends on an external media/TLS library:\n${output}`);
  }
}
