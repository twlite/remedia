/** Session controls for a trusted raw FFmpeg audio filtergraph, independent of master gain. */
export class FilterControls {
  readonly #read: () => string | null;
  readonly #set: (graph: string) => Promise<void>;
  readonly #clear: () => Promise<void>;

  /**
   * Construct this wrapper with its owning services. Use the owning engine or client factory for application code.
   * @param read - Callback returning the currently configured raw filtergraph.
   * @param set - Callback that validates and applies a replacement raw filtergraph.
   * @param clear - Callback that disables the current raw filtergraph.
   */
  constructor(
    read: () => string | null,
    set: (graph: string) => Promise<void>,
    clear: () => Promise<void>,
  ) {
    this.#read = read;
    this.#set = set;
    this.#clear = clear;
  }

  /** Current raw FFmpeg audio filtergraph, or null when filtering is disabled. */
  get graph(): string | null {
    return this.#read();
  }

  /**
   * Validate and replace the persistent raw FFmpeg filtergraph at a safe audio-processing boundary.
   * @param graph - Trusted, non-empty raw FFmpeg audio filtergraph; native validation errors reject the operation.
   * @returns Resolves after configuration is applied; rejects for an empty or invalid filtergraph.
   */
  set(graph: string): Promise<void> {
    if (typeof graph !== 'string' || graph.trim().length === 0) {
      return Promise.reject(new RangeError('filtergraph must be a non-empty string'));
    }
    return this.#set(graph);
  }

  /**
   * Disable the configured raw filtergraph without changing master gain.
   * @returns Resolves when filtering is disabled.
   */
  clear(): Promise<void> {
    return this.#clear();
  }
}

/**
 * Validate a finite linear gain in the inclusive range 0 through 2; invalid values throw RangeError.
 * @param gain - Finite linear master gain from 0 through 2; 1 leaves amplitude unchanged.
 */
export function validateVolume(gain: number): void {
  if (!Number.isFinite(gain) || gain < 0 || gain > 2) {
    throw new RangeError('volume must be a finite linear gain between 0 and 2');
  }
}
