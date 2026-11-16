/** FFmpeg is statically linked into the native addon. */
export type FfmpegSource = 'bundled';

/** One FFmpeg library statically linked into Remedia. */
export interface FfmpegLibraryInfo {
  /** FFmpeg library name without the `lib` prefix, for example `avcodec`. */
  readonly name: string;
  /** Absolute path of the loaded file, or an empty string for a statically linked library. */
  readonly path: string;
  /** Library version reported by the library itself, as `major.minor.micro`. */
  readonly version: string;
  /** ABI major version, which must match the FFmpeg 9 requirement for `name`. */
  readonly major: number;
}

/** The FFmpeg runtime that `@remedia/node` loaded. */
export interface FfmpegRuntimeInfo {
  /** Names compiled into the bundle, without opening media or starting playback. */
  readonly capabilities: {
    /** Available audio decoders. */
    readonly decoders: readonly string[];
    /** Available filtergraph filters. */
    readonly filters: readonly string[];
    /** Available input protocols. */
    readonly protocols: readonly string[];
  };
  /** Actual bundled FFmpeg release version. */
  readonly version: string;
  /** Actual FFmpeg license string. */
  readonly license: string;
  /** Configure arguments compiled into FFmpeg. */
  readonly configuration: string;
  /** Remedia release version compiled into the addon. */
  readonly remediaVersion: string;
  /** Remedia source revision compiled into the addon. */
  readonly remediaCommit: string;
  /** Always bundled; runtime discovery is unnecessary. */
  readonly source: FfmpegSource;
  /** Always null because no separate FFmpeg library files are loaded. */
  readonly directory: null;
  /** Versions reported by the statically linked FFmpeg libraries. */
  readonly libraries: readonly FfmpegLibraryInfo[];
  /** Always empty; retained for callers displaying runtime diagnostics. */
  readonly considered: readonly never[];
}
