export class FilterControls {
  readonly #read: () => string | null;
  readonly #set: (graph: string) => Promise<void>;
  readonly #clear: () => Promise<void>;

  constructor(
    read: () => string | null,
    set: (graph: string) => Promise<void>,
    clear: () => Promise<void>,
  ) {
    this.#read = read;
    this.#set = set;
    this.#clear = clear;
  }

  get graph(): string | null {
    return this.#read();
  }

  set(graph: string): Promise<void> {
    if (typeof graph !== 'string' || graph.trim().length === 0) {
      return Promise.reject(new RangeError('filtergraph must be a non-empty string'));
    }
    return this.#set(graph);
  }

  clear(): Promise<void> {
    return this.#clear();
  }
}

export function validateVolume(gain: number): void {
  if (!Number.isFinite(gain) || gain < 0 || gain > 2) {
    throw new RangeError('volume must be a finite linear gain between 0 and 2');
  }
}
