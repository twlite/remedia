import { readFileSync } from 'node:fs';

// An allowlist prevents host-installed optional libraries from silently changing licensing.
const configuration = readFileSync(process.argv[2], 'utf8');
for (const required of [
  '--enable-gpl',
  '--enable-version3',
  '--enable-static',
  '--disable-shared',
  '--disable-autodetect',
]) {
  if (!configuration.includes(required)) throw new Error(`Missing FFmpeg policy flag ${required}`);
}
const features = configuration.replace(/--enable-(?:decoder|encoder|filter)=['"]?[\w,]+['"]?/g, '');
for (const option of features.match(/--enable-[\w-]+/g) ?? []) {
  if (
    ![
      '--enable-gpl',
      '--enable-version3',
      '--enable-static',
      '--enable-pic',
      '--enable-network',
      '--enable-cross-compile',
      '--enable-openssl',
      '--enable-securetransport',
      '--enable-schannel',
    ].includes(option)
  ) {
    throw new Error(`Unaudited FFmpeg dependency/feature: ${option}`);
  }
}
if (configuration.includes('CONFIG_GPL=')) {
  for (const key of ['GPL', 'VERSION3']) {
    if (!new RegExp(`^CONFIG_${key}=yes$`, 'm').test(configuration))
      throw new Error(`FFmpeg CONFIG_${key} is disabled`);
  }
  if (!/^!CONFIG_NONFREE=yes$/m.test(configuration))
    throw new Error('FFmpeg nonfree code is enabled or not recorded');
  for (const entry of configuration.match(/^CONFIG_LIB\w+=yes$/gm) ?? []) {
    throw new Error(`Unaudited external FFmpeg library: ${entry}`);
  }
}
