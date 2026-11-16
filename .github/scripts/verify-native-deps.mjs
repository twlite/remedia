import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { ffmpegAbi as expectedMajors } from './ffmpeg-abi.mjs';

if (process.argv[2] && process.argv[2] !== '--build')
  throw new Error('Usage: verify-native-deps.mjs [--build]');

const ffmpegDir = process.env.REMEDIA_STATIC_FFMPEG_DIR ?? process.env.FFMPEG_DIR;
if (!ffmpegDir) throw new Error('FFMPEG_DIR must identify the FFmpeg prefix');
console.log(`FFMPEG_DIR: ${ffmpegDir}`);

for (const [library, expectedMajor] of expectedMajors) {
  const version = pkgConfig('--modversion', library);
  const libdir = pkgConfig('--variable=libdir', library);
  console.log(`${library}: ${version} (${libdir})`);
  if (version.split('.')[0] !== expectedMajor) {
    throw new Error(`${library} must have FFmpeg 9 ABI major ${expectedMajor}; found ${version}`);
  }

  if (!existsSync(join(ffmpegDir, 'include', library, `${library.slice(3)}.h`))) {
    throw new Error(`Missing ${library} headers under FFMPEG_DIR=${ffmpegDir}`);
  }
}

{
  const libclangPath = process.env.LIBCLANG_PATH;
  if (!libclangPath)
    throw new Error('LIBCLANG_PATH must identify the build-time libclang directory');
  if (process.platform === 'win32') {
    const libclangDll = join(libclangPath, 'libclang.dll');
    if (!existsSync(libclangDll)) {
      throw new Error(`bindgen requires libclang.dll; expected it at ${libclangDll}`);
    }
    console.log(`bindgen libclang: ${libclangDll}`);
  } else {
    const libclangLibrary = join(
      libclangPath,
      process.platform === 'darwin' ? 'libclang.dylib' : 'libclang.so',
    );
    if (!existsSync(libclangLibrary)) {
      throw new Error(`bindgen libclang library is missing: ${libclangLibrary}`);
    }
    console.log(`bindgen libclang: ${libclangLibrary}`);
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
        throw new Error(`Missing MSVC static library ${join(ffmpegDir, 'lib', filename)}`);
      }
    }
    console.log(`Windows FFmpeg include/lib root: ${ffmpegDir}`);
  }
}

function pkgConfig(...args) {
  return execFileSync('pkg-config', args, { encoding: 'utf8' }).trim();
}
