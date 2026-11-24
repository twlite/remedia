import type { Readable } from 'node:stream';

/** One Opus packet with its 48 kHz output timeline. */
export interface OpusPacket {
  /** Owned encoded packet; retain it as long as your transport needs it. */
  data: Buffer;
  /** Start sample on the continuous output timeline; changes after seek. */
  startSample: number;
  /** Encoded packet duration, including padding, in samples per channel. */
  samplesPerChannel: number;
  /** Media duration excluding padding and pre-roll, in samples per channel. */
  mediaSamplesPerChannel: number;
}

/** Node Readable whose asynchronous iterator yields the selected output type.
 * @typeParam T - Buffer for PCM, or OpusPacket for object-mode output.
 */
export interface AudioReadable<T> extends Readable {
  /** Iterate with demand-driven backpressure; leaving iteration destroys the stream.
   * @returns Iterator of owned audio payloads.
   */
  [Symbol.asyncIterator](): NodeJS.AsyncIterator<T>;
}

/** Output configuration for standalone embedded playback. */
export interface StreamOutputOptions {
  /** PCM is 48 kHz stereo; Opus preserves packet boundaries. */
  format: 's16le' | 'f32le' | 'opus';
  /** Demand-driven production by default; realtime follows the output sample clock. */
  pacing?: 'unpaced' | 'realtime';
  /** PCM-to-Opus encoder settings; source passthrough remains unchanged. */
  opus?: {
    /** Target bits per second, 500–512000; default 128000. */
    bitrate?: number;
    /** Encoder complexity 0–10; default 9. */
    complexity?: number;
    /** Variable bitrate; default true. */
    vbr?: boolean;
  };
}
