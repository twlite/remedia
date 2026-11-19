// The generated napi-rs loader resolves and loads the addon synchronously through `createRequire`,
// so a static import keeps this package free of top-level await and loadable through `require()`.
// Load failures still surface while this module evaluates, with the loader's `cause` chain intact.
import {
  bundledFfmpegInfo,
  NativeRemedia as NativeRemediaBinding,
  type NativeRemedia as NativeRemediaClass,
} from '../native.js';
import type { FfmpegRuntimeInfo } from './ffmpeg-runtime.js';

/** The native engine with statically linked FFmpeg. */
export const NativeRemedia = NativeRemediaBinding;

/** Instance type of the native engine. */
export type NativeRemedia = NativeRemediaClass;

/**
 * Return the bundled FFmpeg identity. No discovery or downloads occur.
 *
 * @returns Bundled library versions, build configuration, license and Remedia identity.
 */
export function getFFmpegRuntime(): FfmpegRuntimeInfo {
  const info = JSON.parse(bundledFfmpegInfo()) as {
    capabilities: FfmpegRuntimeInfo['capabilities'];
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
