import type { BeforePlayContext, BeforePlayHook, MediaSource } from './types.js';

/** Ordered, awaited local middleware, intentionally separate from lifecycle event listeners. */
export class PlaybackHooks<Metadata = unknown> {
  readonly #beforePlay: BeforePlayHook<Metadata>[] = [];

  beforePlay(hook: BeforePlayHook<Metadata>): () => void {
    if (typeof hook !== 'function') throw new TypeError('beforePlay hook must be a function');
    this.#beforePlay.push(hook);
    return () => {
      const index = this.#beforePlay.indexOf(hook);
      if (index >= 0) this.#beforePlay.splice(index, 1);
    };
  }

  async runBeforePlay(
    source: MediaSource,
    customMetadata: Metadata | undefined,
    signal: AbortSignal,
  ): Promise<void> {
    const context: BeforePlayContext<Metadata> = { source, customMetadata, signal };
    for (const hook of [...this.#beforePlay]) {
      throwIfAborted(signal);
      const result = await raceAbort(
        Promise.resolve().then(() => hook(context)),
        signal,
      );
      throwIfAborted(signal);
      if (result?.cancel) {
        const error = new Error(result.reason ?? 'playback was cancelled by a beforePlay hook');
        error.name = 'AbortError';
        throw error;
      }
    }
  }

  /** @internal */
  clear(): void {
    this.#beforePlay.length = 0;
  }
}

function throwIfAborted(signal: AbortSignal): void {
  if (!signal.aborted) return;
  throw abortError(signal.reason);
}

function abortError(reason: unknown): Error {
  const error = new Error(reason instanceof Error ? reason.message : 'playback was cancelled');
  error.name = 'AbortError';
  return error;
}

function raceAbort<Result>(operation: Promise<Result>, signal: AbortSignal): Promise<Result> {
  if (signal.aborted) return Promise.reject(abortError(signal.reason));
  return new Promise((resolve, reject) => {
    const onAbort = (): void => reject(abortError(signal.reason));
    signal.addEventListener('abort', onAbort, { once: true });
    operation.then(resolve, reject).finally(() => signal.removeEventListener('abort', onAbort));
  });
}
