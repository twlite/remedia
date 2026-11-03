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
if (!ffmpegDir) throw new Error('FFMPEG_DIR must identify the FFmpeg prefix');
console.log(`FFMPEG_DIR: ${ffmpegDir}`);

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

if (process.platform === 'win32') {
  const libclangPath = process.env.LIBCLANG_PATH;
  if (!libclangPath) throw new Error('LIBCLANG_PATH must identify the Windows libclang directory');
  const libclangDll = join(libclangPath, 'libclang.dll');
  if (!existsSync(libclangDll)) {
    throw new Error(`bindgen requires libclang.dll; expected it at ${libclangDll}`);
  }
  console.log(`bindgen libclang: ${libclangDll}`);

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
  console.log(`Windows FFmpeg include/lib root: ${ffmpegDir}`);
}

function pkgConfig(...args) {
  return execFileSync('pkg-config', args, { encoding: 'utf8' }).trim();
}
