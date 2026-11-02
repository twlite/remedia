/** Controls one playback session's trusted raw FFmpeg audio filtergraph. */
export class FilterControls {
  readonly #read: () => string | null;
  readonly #replace: (graph: string) => Promise<void>;
  readonly #clear: () => Promise<void>;

  /** @internal Constructed by playback session wrappers. */
  constructor(
    read: () => string | null,
    replace: (graph: string) => Promise<void>,
    clear: () => Promise<void>,
  ) {
    this.#read = read;
    this.#replace = replace;
    this.#clear = clear;
  }

  /** Current configured raw filtergraph, or null when filtering is disabled. */
  get graph(): string | null {
    return this.#read();
  }

  /** Validate and replace the graph at a safe PCM boundary. */
  set(graph: string): Promise<void> {
    if (typeof graph !== 'string') throw new TypeError('filtergraph must be a string');
    if (graph.trim().length === 0) {
      throw new RangeError('filtergraph must not be empty or whitespace-only');
    }
    if (this.graph === graph) return Promise.resolve();
    return this.#replace(graph);
  }

  /** Disable the raw filtergraph without changing master volume. */
  clear(): Promise<void> {
    return this.#clear();
  }
}

export function validateVolume(gain: number): void {
  if (!Number.isFinite(gain) || gain < 0 || gain > 2) {
    throw new RangeError('volume must be a finite linear gain between 0.0 and 2.0');
  }
}
