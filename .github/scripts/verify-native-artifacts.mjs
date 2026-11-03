import { readdirSync } from 'node:fs';
import { basename } from 'node:path';
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
const binaries = [`${packageDirectory}/${addons[0]}`, serverBinary];
console.log(`Testing native addon ${addons[0]} and server ${basename(serverBinary)}`);

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
    requireDependencies(
      dependencies,
      [
        'libavcodec.so.61',
        'libavformat.so.61',
        'libavutil.so.59',
        'libavfilter.so.10',
        'libswresample.so.5',
        'libopus.so.0',
      ],
      binary,
    );
  }
} else if (process.platform === 'darwin') {
  for (const binary of binaries) {
    const architecture = execFileSync('lipo', ['-info', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('otool', ['-L', binary], { encoding: 'utf8' });
    console.log(`${architecture}\n${dependencies}`);
    if (!architecture.includes(process.arch === 'arm64' ? 'arm64' : 'x86_64')) {
      throw new Error(`Unexpected Mach-O architecture for ${binary}`);
    }
    requireDependencies(
      dependencies,
      [
        'libavcodec.61',
        'libavformat.61',
        'libavutil.59',
        'libavfilter.10',
        'libswresample.5',
        'libopus.0',
      ],
      binary,
    );
  }
} else if (process.platform === 'win32') {
  for (const binary of binaries) {
    const headers = execFileSync('dumpbin', ['/headers', binary], { encoding: 'utf8' });
    const dependencies = execFileSync('dumpbin', ['/dependents', binary], { encoding: 'utf8' });
    console.log(`${binary}\n${dependencies}`);
    if (!headers.toLowerCase().includes('machine (x64)')) {
      throw new Error(`Unexpected PE architecture for ${binary}`);
    }
    requireDependencies(
      dependencies.toLowerCase(),
      [
        'avcodec-61.dll',
        'avformat-61.dll',
        'avutil-59.dll',
        'avfilter-10.dll',
        'swresample-5.dll',
        'opus.dll',
      ],
      binary,
    );
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
