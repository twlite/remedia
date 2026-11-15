import { execFileSync } from 'node:child_process';
import { readdirSync, statSync } from 'node:fs';
import { resolve } from 'node:path';

const binaries = process.argv.slice(2);
if (!binaries.length) {
  binaries.push(...readdirSync('.').filter((name) => /^node\..*\.node$/.test(name)));
}
if (!binaries.length) throw new Error('No native binaries supplied for static-link audit');
for (const file of binaries) {
  const binary = resolve(file);
  const [command, args] =
    process.platform === 'darwin'
      ? ['otool', ['-L', binary]]
      : process.platform === 'win32'
        ? ['dumpbin', ['/dependents', binary]]
        : ['readelf', ['-d', binary]];
  const dependencies = execFileSync(command, args, { encoding: 'utf8' });
  if (
    /(?:lib)?(?:avcodec|avformat|avutil|avfilter|swresample|swscale|avdevice|opus|ssl|crypto)[-.\d]*(?:\.so|\.dylib|\.dll)/i.test(
      dependencies,
    )
  ) {
    throw new Error(`External media/TLS dependency in ${binary}:\n${dependencies}`);
  }
  console.log(`${binary}: ${statSync(binary).size} bytes\n${dependencies}`);
}
