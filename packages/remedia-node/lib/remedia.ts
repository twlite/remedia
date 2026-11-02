import { NativeRemedia } from '../native.js';
import { DiscordSession } from './discord.js';
import { PlaybackSession } from './playback.js';
import {
  toDiagnosticEvent,
  toNativeVoiceOptions,
  type DebugListener,
  type DiagnosticEvent,
  type VoiceConnectionOptions,
} from './types.js';

/**
 * One embedded Remedia context. Create instances with `await Remedia.create()` so native
 * observability and optional asynchronous OpenTelemetry exporters initialize on NAPI-RS's runtime.
 */
export class Remedia {
  readonly #native: NativeRemedia;
  readonly #debugListeners = new Set<DebugListener>();
  readonly #sessions = new Set<PlaybackSession>();
  #closed = false;

  private constructor(native: NativeRemedia) {
    this.#native = native;
    native.setDebugCallback((event) => {
      const diagnostic = toDiagnosticEvent(event);
      for (const listener of this.#debugListeners) listener(diagnostic);
    });
  }

  /** Create an independent native context using REMEDIA_* and OTEL_* environment settings. */
  static async create(): Promise<Remedia> {
    return new Remedia(await NativeRemedia.create());
  }

  /** Number of diagnostic events dropped by either bounded event queue. */
  get droppedDebugEvents(): number {
    return this.#native.droppedDebugEvents;
  }

  /** Register a structured native diagnostic listener. */
  on(event: 'debug', listener: DebugListener): this {
    this.#assertOpen();
    if (event === 'debug') this.#debugListeners.add(listener);
    return this;
  }

  /** Remove a previously registered diagnostic listener. */
  off(event: 'debug', listener: DebugListener): this {
    if (event === 'debug') this.#debugListeners.delete(listener);
    return this;
  }

  /** Remove all diagnostic listeners. */
  removeAllListeners(event?: 'debug'): this {
    if (event === undefined || event === 'debug') this.#debugListeners.clear();
    return this;
  }

  /** Create a local decoder session that discards its PCM output. */
  createPlaybackSession(): PlaybackSession {
    this.#assertOpen();
    let playback: PlaybackSession;
    playback = new PlaybackSession(this.#native, this.#native.createPlaybackSession(), () => {
      this.#sessions.delete(playback);
    });
    this.#sessions.add(playback);
    return playback;
  }

  /** Connect a voice transport using credentials collected by the application's Discord Gateway. */
  async connectVoice(options: VoiceConnectionOptions): Promise<DiscordSession> {
    this.#assertOpen();
    const result = await this.#native.connectVoice(toNativeVoiceOptions(options));
    let session: DiscordSession;
    session = new DiscordSession(this.#native, result.playbackId, result.voiceId, () => {
      this.#sessions.delete(session);
    });
    this.#sessions.add(session);
    return session;
  }

  /** Stop all playbacks, disconnect transports, flush telemetry, and release native resources. */
  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#debugListeners.clear();
    try {
      await this.#native.close();
    } finally {
      await Promise.all([...this.#sessions].map((session) => session.releaseInput()));
      this.#sessions.clear();
    }
  }

  #assertOpen(): void {
    if (this.#closed) throw new Error('Remedia instance is closed');
  }
}

export type { DebugListener, DiagnosticEvent };
