import { EventEmitter } from 'node:events';
import { RemediaEventEmitter } from './events.js';
import { NativeRemedia } from '../native.js';
import { DiscordSession, type DiscordSessionOptions } from './discord.js';
import { DiscordGatewayAdapter, type DiscordGatewaySend } from './gateway.js';
import type { RemediaLifecycleEvents } from './lifecycle.js';
import {
  PlaybackSessionEvents as PlaybackEvent,
  RemediaEvents as RemediaEvent,
  VoiceSessionEvents as VoiceEvent,
} from './lifecycle.js';
import { PlaybackSession } from './playback.js';
import { toDiagnosticEvent, type DebugListener, type DiagnosticEvent } from './types.js';

/** Optional Gateway integration for the embedded engine; required when creating Discord sessions. */
export interface RemediaOptions {
  /** Send raw Gateway payloads through the application's existing Discord client or shard. */
  send: DiscordGatewaySend;
}

interface PlaybackResource {
  dispose(): Promise<void>;
  releaseInput(): Promise<void>;
}

interface DiscordSessionResource {
  readonly guildId: string;
  receiveGatewayDispatch(
    event: string,
    data: Record<string, unknown>,
    sequence?: number,
    epoch?: number,
  ): void;
  dispose(): Promise<void>;
}

/**
 * One embedded Remedia context. Create instances with `await Remedia.create()` so native
 * observability and optional asynchronous OpenTelemetry exporters initialize on NAPI-RS's runtime.
 */
export class Remedia extends RemediaEventEmitter<RemediaLifecycleEvents> {
  readonly #native: NativeRemedia;
  readonly #sessions = new Set<PlaybackResource>();
  readonly #discordSessions = new Map<string, DiscordSessionResource>();
  readonly #gateway: DiscordGatewayAdapter | undefined;
  #closed = false;

  private constructor(native: NativeRemedia, options?: RemediaOptions) {
    super();
    this.#native = native;
    this.#gateway = options ? new DiscordGatewayAdapter(options.send) : undefined;
    native.setDebugCallback((event) => {
      const diagnostic = toDiagnosticEvent(event);
      this.emit(RemediaEvent.debug, diagnostic);
    });
  }

  /**
   * Create an independent native context using REMEDIA_* and OTEL_* environment settings.
   * @param options - Optional bot Gateway callback; required when creating Discord sessions.
   * @returns An initialized independent native engine context.
   */
  static async create(options?: RemediaOptions): Promise<Remedia> {
    return new Remedia(await NativeRemedia.create(), options);
  }

  /** Number of diagnostic events dropped by either bounded event queue. */
  get droppedDebugEvents(): number {
    return this.#native.droppedDebugEvents;
  }

  /**
   * Create a local decoder session that discards its PCM output.
   * @returns An independent playback session owned by this engine until disposal.
   * @typeParam Metadata - Application-owned metadata attached to playback generations.
   */
  createPlaybackSession<Metadata = unknown>(): PlaybackSession<Metadata> {
    this.#assertOpen();
    let playback: PlaybackSession<Metadata>;
    playback = new PlaybackSession<Metadata>(
      this.#native,
      this.#native.createPlaybackSession(),
      () => {
        this.#sessions.delete(playback);
      },
    );
    forwardLifecycleEvents(playback, this);
    this.#sessions.add(playback);
    return playback;
  }

  /**
   * Create a Gateway-managed voice session. Call `connect()` to send OP 4 and join the channel.
   * @param options - Guild and bot identity, requested channel, initial processing settings, and voice flags.
   * @returns The unconnected session; call connect() to request a Gateway voice join.
   * @typeParam Metadata - Application-owned metadata attached to playback generations.
   */
  createDiscordSession<Metadata = unknown>(
    options: DiscordSessionOptions,
  ): DiscordSession<Metadata> {
    this.#assertOpen();
    if (!this.#gateway) {
      throw new Error('Remedia.create() requires a Gateway send function for Discord sessions');
    }
    if (this.#discordSessions.has(options.guildId)) {
      throw new Error(`a Discord voice session already exists for guild ${options.guildId}`);
    }
    let session: DiscordSession<Metadata>;
    session = new DiscordSession<Metadata>(this.#native, this.#gateway, options, () => {
      this.#discordSessions.delete(session.guildId);
    });
    forwardLifecycleEvents(session, this);
    this.#discordSessions.set(session.guildId, session);
    return session;
  }

  /**
   * Forward a raw Discord Gateway packet to the matching in-process voice session.
   * @param packet - Raw packet from this bot's Gateway dispatch stream.
   */
  receive(packet: unknown): void {
    if (this.#closed || !this.#gateway) return;
    const dispatch = this.#gateway.receive(packet);
    if (!dispatch) return;
    this.#discordSessions
      .get(dispatch.guildId)
      ?.receiveGatewayDispatch(dispatch.event, dispatch.data, dispatch.sequence, dispatch.epoch);
  }

  /**
   * Stop all playbacks, disconnect transports, flush telemetry, and release native resources.
   * @returns Resolves after cleanup; rejects when a resource fails to close.
   */
  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    const voiceCleanup = await Promise.allSettled(
      [...this.#discordSessions.values()].map((session) => session.dispose()),
    );
    try {
      await Promise.all([...this.#sessions].map((session) => session.dispose()));
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

function forwardLifecycleEvents<Metadata>(
  source: PlaybackSession<Metadata> | DiscordSession<Metadata>,
  target: Remedia,
): void {
  const sourceEvents = source as unknown as EventEmitter;
  const targetEvents = target as unknown as EventEmitter;
  const names = [
    PlaybackEvent.playbackStarted,
    PlaybackEvent.playbackCompleted,
    PlaybackEvent.playbackStopped,
    PlaybackEvent.playbackFailed,
    PlaybackEvent.playbackPaused,
    PlaybackEvent.playbackResumed,
    PlaybackEvent.playbackSeeked,
    PlaybackEvent.filterChanged,
    PlaybackEvent.volumeChanged,
    PlaybackEvent.sessionDisposed,
    ...(source instanceof DiscordSession
      ? [VoiceEvent.voiceStateChanged, VoiceEvent.voiceFailed]
      : []),
  ] as const;
  const unsubs: Array<() => void> = [];
  for (const name of names) {
    const listener = (event: unknown): void => {
      targetEvents.emit(name, event);
    };
    sourceEvents.on(name, listener);
    unsubs.push(() => sourceEvents.off(name, listener));
  }
  const detach = (): void => {
    sourceEvents.off(PlaybackEvent.sessionDisposed, detach);
    for (const unsubscribe of unsubs.splice(0)) unsubscribe();
  };
  sourceEvents.once(PlaybackEvent.sessionDisposed, detach);
}

export type { DebugListener, DiagnosticEvent };
