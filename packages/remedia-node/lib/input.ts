import { Buffer } from 'node:buffer';
import type { Readable } from 'node:stream';

import type { NativeMediaSource, NativeRemedia } from '../native.js';

export type MediaSource =
  | { file: string }
  | { url: string }
  | { buffer: Buffer | Uint8Array }
  | { stream: Readable };

export interface ReadableInputHandle {
  readonly source: NativeMediaSource;
  cancel(): Promise<void>;
}

export function toNativeSource(
  source: Exclude<MediaSource, { stream: Readable }>,
): NativeMediaSource {
  if ('file' in source) return { kind: 'file', value: source.file };
  if ('url' in source) return { kind: 'url', value: source.url };
  return { kind: 'buffer', bytes: Buffer.from(source.buffer) };
}

export function createReadableInput(native: NativeRemedia, stream: Readable): ReadableInputHandle {
  const streamId = native.createByteStream();
  const cancellation = new AbortController();
  const pump = pumpReadable(native, streamId, stream, cancellation.signal);
  let cleanup: Promise<void> | undefined;

  return {
    source: { kind: 'stream', value: streamId },
    cancel() {
      cleanup ??= (async () => {
        cancellation.abort();
        native.cancelByteStream(streamId);
        await pump;
      })();
      return cleanup;
    },
  };
}

type ReadableWake = 'readable' | 'end' | 'close' | 'abort';

async function pumpReadable(
  native: NativeRemedia,
  streamId: string,
  stream: Readable,
  signal: AbortSignal,
): Promise<void> {
  let sourceError: Error | undefined;
  let failureSent = false;
  const failSource = (error: Error): void => {
    sourceError ??= error;
    if (failureSent || signal.aborted) return;
    failureSent = true;
    void native.failByteStream(streamId, sourceError.message).catch(() => {});
  };
  const onError = (error: Error): void => failSource(error);

  stream.pause();
  stream.on('error', onError);
  try {
    while (!signal.aborted) {
      if (sourceError) throw sourceError;
      if (stream.errored) throw stream.errored;

      const chunk: unknown = stream.read();
      if (chunk === null) {
        const wake = await waitForReadable(stream, signal, () => sourceError);
        if (wake === 'abort') return;
        if (wake === 'end') break;
        if (wake === 'close') throw sourceError ?? new Error('Readable closed before ending');
        continue;
      }

      if (!(chunk instanceof Uint8Array)) {
        throw new TypeError('Readable media sources must produce Buffer or Uint8Array chunks');
      }
      // This copy gives native code an independent snapshot of the chunk. The
      // byte queue then takes ownership and waits for decoder backpressure.
      await native.writeByteStream(streamId, Buffer.from(chunk));
    }

    if (!signal.aborted && !sourceError) await native.finishByteStream(streamId);
  } catch (error) {
    if (!signal.aborted) {
      failSource(error instanceof Error ? error : new Error(String(error)));
    }
  } finally {
    stream.pause();
    stream.off('error', onError);
  }
}

function waitForReadable(
  stream: Readable,
  signal: AbortSignal,
  getError: () => Error | undefined,
): Promise<ReadableWake> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const cleanup = (): void => {
      stream.off('readable', onReadable);
      stream.off('end', onEnd);
      stream.off('close', onClose);
      stream.off('error', onError);
      signal.removeEventListener('abort', onAbort);
    };
    const finish = (wake: ReadableWake): void => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(wake);
    };
    const fail = (error: Error): void => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };
    const onReadable = (): void => finish('readable');
    const onEnd = (): void => finish('end');
    const onClose = (): void => finish('close');
    const onError = (error: Error): void => fail(error);
    const onAbort = (): void => finish('abort');

    stream.once('readable', onReadable);
    stream.once('end', onEnd);
    stream.once('close', onClose);
    stream.once('error', onError);
    signal.addEventListener('abort', onAbort, { once: true });

    // An event can arrive between read() returning null and listener setup.
    if (signal.aborted) onAbort();
    else if (getError()) onError(getError()!);
    else if (stream.errored) onError(stream.errored);
    else if (stream.readableEnded) onEnd();
    else if (stream.destroyed) onClose();
    else if (stream.readableLength > 0) onReadable();
  });
}
