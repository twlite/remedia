import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';

const expectedMajors = new Map([
  ['libavcodec', '61'],
  ['libavformat', '61'],
  ['libavutil', '59'],
  ['libavfilter', '10'],
  ['libswresample', '5'],
]);

const ffmpegDir = process.env.FFMPEG_DIR;
const opusDir = process.env.OPUS_LIB_DIR ?? process.env.LIBOPUS_LIB_DIR;
if (!ffmpegDir || !opusDir) {
  throw new Error('FFMPEG_DIR and OPUS_LIB_DIR must identify the FFmpeg and Opus prefixes');
}
console.log(`FFMPEG_DIR: ${ffmpegDir}`);
console.log(`OPUS_LIB_DIR: ${opusDir}`);

const ffmpegVersion = execFileSync('ffmpeg', ['-version'], { encoding: 'utf8' }).split('\n')[0];
console.log(`FFmpeg executable: ${ffmpegVersion}`);
if (!/^ffmpeg version 7\./i.test(ffmpegVersion)) {
  throw new Error(`FFmpeg 7 is required, received: ${ffmpegVersion}`);
}

for (const [library, expectedMajor] of expectedMajors) {
  const version = pkgConfig('--modversion', library);
  const libdir = pkgConfig('--variable=libdir', library);
  console.log(`${library}: ${version} (${libdir})`);
  if (version.split('.')[0] !== expectedMajor) {
    throw new Error(`${library} must have FFmpeg 7 ABI major ${expectedMajor}; found ${version}`);
  }
  if (!existsSync(join(ffmpegDir, 'include', library, `${library.slice(3)}.h`))) {
    throw new Error(`Missing ${library} headers under FFMPEG_DIR=${ffmpegDir}`);
  }
}

const opusVersion = pkgConfig('--modversion', 'opus');
const opusLibdir = pkgConfig('--variable=libdir', 'opus');
console.log(`opus: ${opusVersion} (${opusLibdir})`);
if (!opusVersion.startsWith('1.')) throw new Error(`Unexpected libopus version: ${opusVersion}`);
if (!existsSync(join(opusDir, 'lib'))) {
  throw new Error(`Missing the lib directory under OPUS_LIB_DIR=${opusDir}`);
}
if (!existsSync(join(opusDir, 'include', 'opus', 'opus.h'))) {
  throw new Error(`Missing the Opus headers under OPUS_LIB_DIR=${opusDir}`);
}

if (process.platform === 'win32') {
  for (const filename of [
    'avcodec.lib',
    'avformat.lib',
    'avutil.lib',
    'avfilter.lib',
    'swresample.lib',
  ]) {
    if (!existsSync(join(ffmpegDir, 'lib', filename))) {
      throw new Error(`Missing MSVC import library ${join(ffmpegDir, 'lib', filename)}`);
    }
  }
  if (!existsSync(join(opusDir, 'lib', 'opus.lib'))) {
    throw new Error(`Missing MSVC Opus import library ${join(opusDir, 'lib', 'opus.lib')}`);
  }
  console.log(`Windows FFmpeg include/lib root: ${ffmpegDir}`);
  console.log(`Windows Opus include/lib root: ${opusDir}`);
}

function pkgConfig(...args) {
  return execFileSync('pkg-config', args, { encoding: 'utf8' }).trim();
}
