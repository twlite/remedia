import { createRequire } from 'node:module';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  type BootstrapBinding,
  type FfmpegRuntimeInfo,
  UNIX_INSTALL_COMMAND,
  WINDOWS_INSTALL_COMMAND,
  selectFfmpeg,
} from './ffmpeg-runtime.js';
import { bootstrapCandidates, platformSuffixes } from './bootstrap-locator.js';
import type { NativeRemedia as NativeRemediaClass } from '../native.js';

const require = createRequire(import.meta.url);

/** Interface version of the bootstrap addon this loader was written against. */
const BOOTSTRAP_VERSION = 1;

function locateBootstrap(): BootstrapBinding | null {
  const packageRoot = dirname(dirname(fileURLToPath(import.meta.url)));
  const suffixes = platformSuffixes(process.platform, process.arch);

  for (const candidate of bootstrapCandidates(packageRoot, suffixes)) {
    try {
      const binding = require(candidate) as BootstrapBinding;

      if (binding.bootstrapVersion() === BOOTSTRAP_VERSION) return binding;
    } catch {
      // An unusable bootstrap is treated like a missing one; the OS loader is the fallback.
    }
  }

  return null;
}

function looksLikeMissingLibrary(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);

  return /libav|libsw|avcodec|avformat|avutil|avfilter|swresample|ffmpeg|\.dylib|\.so\b|specified module|dll/i.test(
    message,
  );
}

let runtime: FfmpegRuntimeInfo | null = null;

async function loadNativeBinding(): Promise<typeof import('../native.js')> {
  const bootstrap = locateBootstrap();
  const selected: FfmpegRuntimeInfo = bootstrap
    ? selectFfmpeg(bootstrap)
    : { source: 'unmanaged', directory: null, libraries: [], considered: [] };

  let binding: typeof import('../native.js');

  try {
    binding = await import('../native.js');
  } catch (error) {
    const hint =
      selected.source === 'unmanaged' && looksLikeMissingLibrary(error)
        ? `\n\nThis platform has no Remedia library bootstrap, so the system library loader must find FFmpeg 9 itself. ` +
          `Install it with the Remedia installer (${
            process.platform === 'win32' ? WINDOWS_INSTALL_COMMAND : UNIX_INSTALL_COMMAND
          }) and add its library directory to your loader search path.`
        : '';
    const origin =
      selected.source === 'unmanaged'
        ? 'The Remedia native addon could not be loaded'
        : `FFmpeg 9 was loaded from ${selected.directory} (${selected.source}), but the Remedia native addon could not be loaded`;

    throw new Error(`${origin}: ${error instanceof Error ? error.message : String(error)}${hint}`, {
      cause: error,
    });
  }

  // Only now that the engine addon has loaded against the libraries is the runtime reported.
  runtime = Object.freeze({ ...selected });

  return binding;
}

const binding = await loadNativeBinding();

/** The Remedia native engine class, loaded after FFmpeg 9 was located. */
export const NativeRemedia = binding.NativeRemedia;

/** Instance type of the Remedia native engine. */
export type NativeRemedia = NativeRemediaClass;

/**
 * Report the FFmpeg 9 installation `@remedia/node` loaded.
 *
 * Importing the package already selects and loads FFmpeg, so this never triggers a search or a
 * download and is cheap to call. It is independent of Discord, playback and the standalone server.
 *
 * @returns The selected source and directory, the loaded library files and versions, and every
 * candidate discovery examined. When the platform has no library bootstrap the source is
 * `unmanaged` and the library list is empty.
 * @throws {Error} Only if called before the native addon finished loading, which cannot happen
 * after the package has been imported successfully.
 * @example
 * ```ts
 * import { getFfmpegRuntime } from '@remedia/node';
 *
 * const { source, directory, libraries } = getFfmpegRuntime();
 * console.log(source, directory, libraries.map((library) => `${library.name} ${library.version}`));
 * ```
 */
export function getFfmpegRuntime(): FfmpegRuntimeInfo {
  if (!runtime) throw new Error('The Remedia native addon has not finished loading.');

  return runtime;
}
