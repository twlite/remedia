import { NativeRemedia } from '../native.js';
import { DiscordSession, type DiscordSessionOptions } from './discord.js';
import { DiscordGatewayAdapter, type DiscordGatewaySend } from './gateway.js';
import { PlaybackSession } from './playback.js';
import { toDiagnosticEvent, type DebugListener, type DiagnosticEvent } from './types.js';

export interface RemediaOptions {
  /** Send raw Gateway payloads through the application's existing Discord client or shard. */
  send: DiscordGatewaySend;
}

/**
 * One embedded Remedia context. Create instances with `await Remedia.create()` so native
 * observability and optional asynchronous OpenTelemetry exporters initialize on NAPI-RS's runtime.
 */
export class Remedia {
  readonly #native: NativeRemedia;
  readonly #debugListeners = new Set<DebugListener>();
  readonly #sessions = new Set<PlaybackSession>();
  readonly #discordSessions = new Map<string, DiscordSession>();
  readonly #gateway: DiscordGatewayAdapter | undefined;
  #closed = false;

  private constructor(native: NativeRemedia, options?: RemediaOptions) {
    this.#native = native;
    this.#gateway = options ? new DiscordGatewayAdapter(options.send) : undefined;
    native.setDebugCallback((event) => {
      const diagnostic = toDiagnosticEvent(event);
      for (const listener of this.#debugListeners) listener(diagnostic);
    });
  }

  /** Create an independent native context using REMEDIA_* and OTEL_* environment settings. */
  static async create(options?: RemediaOptions): Promise<Remedia> {
    return new Remedia(await NativeRemedia.create(), options);
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

  /** Create a Gateway-managed voice session. Call `connect()` to send OP 4 and join the channel. */
  createDiscordSession(options: DiscordSessionOptions): DiscordSession {
    this.#assertOpen();
    if (!this.#gateway) {
      throw new Error('Remedia.create() requires a Gateway send function for Discord sessions');
    }
    if (this.#discordSessions.has(options.guildId)) {
      throw new Error(`a Discord voice session already exists for guild ${options.guildId}`);
    }
    let session: DiscordSession;
    session = new DiscordSession(this.#native, this.#gateway, options, () => {
      this.#discordSessions.delete(session.guildId);
    });
    this.#discordSessions.set(session.guildId, session);
    return session;
  }

  /** Forward a raw Discord Gateway packet to the matching in-process voice session. */
  receive(packet: unknown): void {
    if (this.#closed || !this.#gateway) return;
    const dispatch = this.#gateway.receive(packet);
    if (!dispatch) return;
    this.#discordSessions
      .get(dispatch.guildId)
      ?.receiveGatewayDispatch(dispatch.event, dispatch.data, dispatch.sequence);
  }

  /** Stop all playbacks, disconnect transports, flush telemetry, and release native resources. */
  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#debugListeners.clear();
    const voiceCleanup = await Promise.allSettled(
      [...this.#discordSessions.values()].map((session) => session.dispose()),
    );
    try {
      await this.#native.close();
    } finally {
      await Promise.all([...this.#sessions].map((session) => session.releaseInput()));
      this.#sessions.clear();
      this.#discordSessions.clear();
    }
    const failures = voiceCleanup.flatMap((result) =>
      result.status === 'rejected' ? [result.reason] : [],
    );
    if (failures.length > 0) {
      throw new AggregateError(failures, 'one or more Discord voice sessions failed to close');
    }
  }

  #assertOpen(): void {
    if (this.#closed) throw new Error('Remedia instance is closed');
  }
}

export type { DebugListener, DiagnosticEvent };
