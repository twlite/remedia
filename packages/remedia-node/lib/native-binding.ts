import type { NativeRemedia as NativeRemediaClass } from '../native.js';
import type { FfmpegRuntimeInfo } from './ffmpeg-runtime.js';

const binding = await import('../native.js');

/** The native engine with statically linked FFmpeg. */
export const NativeRemedia = binding.NativeRemedia;

/** Instance type of the native engine. */
export type NativeRemedia = NativeRemediaClass;

/**
 * Return the bundled FFmpeg identity. No discovery or downloads occur.
 *
 * @returns Bundled library versions, build configuration, license and Remedia identity.
 */
export function getFfmpegRuntime(): FfmpegRuntimeInfo {
  const info = JSON.parse(binding.bundledFfmpegInfo()) as {
    version: string;
    license: string;
    configuration: string;
    remediaVersion: string;
    remediaCommit: string;
    libraries: [string, number][];
  };

  return {
    ...info,
    source: 'bundled',
    directory: null,
    considered: [],
    libraries: info.libraries.map(([name, packed]) => ({
      name,
      path: '',
      major: packed >>> 16,
      version: `${packed >>> 16}.${(packed >>> 8) & 0xff}.${packed & 0xff}`,
    })),
  };
}
