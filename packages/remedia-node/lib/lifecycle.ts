import type { MediaSource } from './input.js';
import type { AudioStreamInfo, PlaybackPosition, PlaybackState } from './types.js';

/** Stable names for playback/session event subscriptions and emission. */
export const PlaybackSessionEvents = {
  /** Emitted after a source starts and its stream information is available. */
  playbackStarted: 'playbackStarted',
  /** Emitted when a source generation reaches natural end-of-stream. */
  playbackCompleted: 'playbackCompleted',
  /** Emitted when a source generation is explicitly stopped or cancelled. */
  playbackStopped: 'playbackStopped',
  /** Emitted when a source generation fails to open, decode, process, or deliver output. */
  playbackFailed: 'playbackFailed',
  /** Emitted when playback is paused without replacing its source generation. */
  playbackPaused: 'playbackPaused',
  /** Emitted when paused playback resumes. */
  playbackResumed: 'playbackResumed',
  /** Emitted after seeking to a source timeline position. */
  playbackSeeked: 'playbackSeeked',
  prepareRequested: 'prepareRequested',
  stageReady: 'stageReady',
  stageFailed: 'stageFailed',
  stageInvalidated: 'stageInvalidated',
  transitionCancelled: 'transitionCancelled',
  /** Emitted when the configured session filtergraph changes. */
  filterChanged: 'filterChanged',
  /** Emitted when the configured session linear gain changes. */
  volumeChanged: 'volumeChanged',
  /** Emitted when a session is unregistered and its owned resources are released. */
  sessionDisposed: 'sessionDisposed',
} as const;

/** Union of the stable event names defined by PlaybackSessionEvents. */
export type PlaybackSessionEvents =
  (typeof PlaybackSessionEvents)[keyof typeof PlaybackSessionEvents];

/** Stable event names for typed VoiceSessionEvents subscriptions. */
export const VoiceSessionEvents = {
  /** Emitted when the Discord transport lifecycle state changes. */
  voiceStateChanged: 'voiceStateChanged',
  /** Emitted when voice connection or recovery fails. */
  voiceFailed: 'voiceFailed',
} as const;

/** Union of the stable event names defined by VoiceSessionEvents. */
export type VoiceSessionEvents = (typeof VoiceSessionEvents)[keyof typeof VoiceSessionEvents];

/** Stable event names for typed RemediaEvents subscriptions. */
export const RemediaEvents = {
  ...PlaybackSessionEvents,
  ...VoiceSessionEvents,
  /** Emitted for a structured native diagnostic event. */
  debug: 'debug',
} as const;

/** Union of the stable event names defined by RemediaEvents. */
export type RemediaEvents = (typeof RemediaEvents)[keyof typeof RemediaEvents];

/**
 * Correlation fields identifying the session and source generation that produced a playback event.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */

/** Session opt-in for continuous PCM crossfade playback. */
export interface CrossfadeOptions {
  /** Explicit enablement; omitted sessions retain ordinary playback. */
  enabled: true;
  /** Overlap duration in milliseconds, 1000–30000; defaults to 5000. */
  durationMs?: number;
  /** Lead before overlap in milliseconds, 5000–120000; defaults to 30000. */
  prepareBeforeMs?: number;
  /** Equal-power curve; the only supported curve. */
  curve?: 'equalPower';
}

/** Identify a particular queued occurrence. Revisions prevent stale stage
 * responses from replacing newer queue decisions.
 */
export interface StageNextOptions<Metadata = unknown> {
  /** Stable ID of this queued occurrence, including repeated tracks. */
  entryId: string;
  /** Monotonically increasing queue decision revision; non-negative safe integer. */
  queueRevision: number;
  /** Revision of the resolved source for this entry; defaults to zero. */
  sourceRevision?: number;
  /** Absolute Unix time in milliseconds after which a prepared source must not be promoted. */
  expiresAtMs?: number;
  /** Current source generation to fence delayed preparation results; defaults to the active generation. */
  expectedGeneration?: number;
  /** Application-owned metadata retained locally and attached to incoming lifecycle events. */
  customMetadata?: Metadata;
}
/** Crossfade playback identity, state, or capability metadata. */
export interface StageSnapshot {
  /** Current source generation to fence delayed preparation results; defaults to the active generation. */
  expectedGeneration: number;
  /** Stable ID of this queued occurrence, including repeated tracks. */
  entryId: string;
  /** Monotonically increasing queue decision revision; non-negative safe integer. */
  queueRevision: number;
  /** Revision of the resolved source for this entry; defaults to zero. */
  sourceRevision: number;
  /** Current preparation state; failed sources cannot be promoted. */
  state: 'preparing' | 'ready' | 'failed';
  /** Decoded PCM currently buffered, in milliseconds; bounded to two seconds. */
  bufferedMs: number;
  /** Selected stream metadata, or null until the decoder opens. */
  info: AudioStreamInfo | null;
  /** Preparation failure message, or null. */
  error: string | null;
}
/** Crossfade playback identity, state, or capability metadata. */
export interface CrossfadeContinuation {
  /** Discriminator identifying this lifecycle transition. */
  type: 'promoted';
  /** Source generation identifying the promoted playback. */
  generation: number;
  /** Stable ID of this queued occurrence, including repeated tracks. */
  entryId: string;
}
/** Crossfade playback identity, state, or capability metadata. */
export interface CrossfadeTransition {
  /** Discriminator identifying this lifecycle transition. */
  type: 'crossfade';
  /** Source generation that remains audible during the fade. */
  outgoingGeneration: number;
  /** Stable ID of this queued occurrence, including repeated tracks. */
  entryId: string;
}
/** Crossfade playback identity, state, or capability metadata. */
export interface CrossfadeStageEvent extends PlaybackEventIdentity {
  /** Stable ID of this queued occurrence, including repeated tracks. */
  entryId: string;
  /** Monotonically increasing queue decision revision; non-negative safe integer. */
  queueRevision: number;
  /** Revision of the resolved source for this entry; defaults to zero. */
  sourceRevision: number;
}

export interface PlaybackEventIdentity<Metadata = unknown> {
  /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
  sessionId: string;
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  guildId?: string;
  /** Node identifier; embedded session events use null. */
  nodeId: string | null;
  /** Source generation used to correlate playback events; independent of transport replacement generations. */
  generation: number;
  /** Application-owned metadata retained locally for the source generation and attached to its events. */
  customMetadata?: Metadata;
  /** True only when a terminal event was reconstructed from an authoritative remote snapshot. */
  reconstructed?: boolean;
}

/**
 * Typed argument tuples for playback lifecycle notifications.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export type PlaybackLifecycleEvents<Metadata = unknown> = {
  /** Emitted after a source starts and its stream information is available. */
  [PlaybackSessionEvents.playbackStarted]: [
    PlaybackEventIdentity<Metadata> & {
      /** Current playback lifecycle state. */
      state: 'playing';
      /** Metadata for the audio stream that started playback. */
      info: AudioStreamInfo;
      transition?: CrossfadeTransition;
    },
  ];
  /** Emitted when a source generation reaches natural end-of-stream. */
  [PlaybackSessionEvents.playbackCompleted]: [
    PlaybackEventIdentity<Metadata> & {
      /** Current playback lifecycle state. */
      state: 'completed';
      /** Latest source and output timeline measurements in milliseconds. */
      position: PlaybackPosition;
      continuation?: CrossfadeContinuation;
    },
  ];
  /** Emitted when a source generation is explicitly stopped or cancelled. */
  [PlaybackSessionEvents.playbackStopped]: [
    PlaybackEventIdentity<Metadata> & {
      /** Current playback lifecycle state. */
      state: 'stopped';
      /** Optional explanation for cancellation or a terminal playback event. */
      reason?: string;
    },
  ];
  /** Emitted when a source generation fails to open, decode, process, or deliver output. */
  [PlaybackSessionEvents.playbackFailed]: [
    PlaybackEventIdentity<Metadata> & {
      /** Current playback lifecycle state. */
      state: 'failed';
      /** Error object describing the operation or source failure. */
      error: Error;
    },
  ];
  /** Emitted when playback is paused without replacing its source generation. */
  [PlaybackSessionEvents.prepareRequested]: [
    PlaybackEventIdentity<Metadata> & { remainingMs: number; transitionInMs: number },
  ];
  [PlaybackSessionEvents.stageReady]: [CrossfadeStageEvent];
  [PlaybackSessionEvents.stageFailed]: [CrossfadeStageEvent & { reason: string }];
  [PlaybackSessionEvents.stageInvalidated]: [CrossfadeStageEvent];
  [PlaybackSessionEvents.transitionCancelled]: [
    PlaybackEventIdentity<Metadata> & { failedGeneration: number; entryId: string },
  ];
  [PlaybackSessionEvents.playbackPaused]: [
    PlaybackEventIdentity<Metadata> & {
      /** Playback or transport lifecycle state before this transition. */
      previousState: PlaybackState;
      /** Current playback lifecycle state. */
      state: 'paused';
    },
  ];
  /** Emitted when paused playback resumes. */
  [PlaybackSessionEvents.playbackResumed]: [
    PlaybackEventIdentity<Metadata> & {
      /** Playback or transport lifecycle state before this transition. */
      previousState: PlaybackState;
      /** Current playback lifecycle state. */
      state: 'playing';
    },
  ];
  /** Emitted after seeking to a source timeline position. */
  [PlaybackSessionEvents.playbackSeeked]: [
    PlaybackEventIdentity<Metadata> & {
      /** Requested source timeline position in milliseconds. */
      positionMs: number;
      /** Source timeline position before seeking, in milliseconds, or null when unknown. */
      previousPositionMs: number | null;
    },
  ];
  /** Emitted when the configured session filtergraph changes. */
  [PlaybackSessionEvents.filterChanged]: [
    PlaybackEventIdentity<Metadata> & {
      /** Filtergraph active before this change, or null when filtering was disabled. */
      previousGraph: string | null;
      /** Current raw FFmpeg audio filtergraph, or null when filtering is disabled. */
      graph: string | null;
    },
  ];
  /** Emitted when the configured session linear gain changes. */
  [PlaybackSessionEvents.volumeChanged]: [
    PlaybackEventIdentity<Metadata> & {
      /** Linear master gain before this change. */
      previousVolume: number;
      /** Session master linear gain from 0 through 2, independent of filtergraph processing; defaults to 1. */
      volume: number;
    },
  ];
};

/** Correlation fields identifying the session that produced a voice transport event. */
export interface VoiceEventIdentity {
  /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
  sessionId: string;
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  guildId: string;
  /** Node identifier; embedded session events use null. */
  nodeId: string | null;
}

/** Normalized voice lifecycle state used in events; DAVE negotiation uses negotiating_dave. */
export type LifecycleVoiceState =
  | 'connecting'
  | 'negotiating_dave'
  | 'connected'
  | 'reconnecting'
  | 'disconnecting'
  | 'disconnected'
  | 'failed';

/** Typed argument tuples for voice transport notifications, independent of playback generation. */
export type VoiceLifecycleEvents = {
  /** Emitted when the Discord transport lifecycle state changes. */
  [VoiceSessionEvents.voiceStateChanged]: [
    VoiceEventIdentity & {
      /** Playback or transport lifecycle state before this transition. */
      previousState: LifecycleVoiceState;
      /** Current playback lifecycle state. */
      state: LifecycleVoiceState;
    },
  ];
  /** Emitted when voice connection or recovery fails. */
  [VoiceSessionEvents.voiceFailed]: [
    VoiceEventIdentity & {
      /** Current playback lifecycle state. */
      state: 'failed';
      /** Error object describing the operation or source failure. */
      error: Error;
    },
  ];
};

/**
 * Combined playback, voice, and disposal event arguments for a session.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export type SessionLifecycleEvents<Metadata = unknown> = PlaybackLifecycleEvents<Metadata> &
  VoiceLifecycleEvents & {
    /** Emitted when a session is unregistered and its owned resources are released. */
    [PlaybackSessionEvents.sessionDisposed]: [
      {
        /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
        sessionId: string;
        /** Discord guild snowflake as a decimal string; unique within the owning client. */
        guildId?: string;
        /** Node identifier; embedded session events use null. */
        nodeId: string | null;
        /** Source generation used to correlate playback events; independent of transport replacement generations. */
        generation: number;
      },
    ];
  };

/**
 * Playback and disposal event arguments emitted by an embedded playback wrapper.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export type PlaybackSessionEventMap<Metadata = unknown> = PlaybackLifecycleEvents<Metadata> & {
  /** Emitted when a session is unregistered and its owned resources are released. */
  [PlaybackSessionEvents.sessionDisposed]: [
    {
      /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
      sessionId: string;
      /** Discord guild snowflake as a decimal string; unique within the owning client. */
      guildId?: string;
      /** Node identifier; embedded session events use null. */
      nodeId: string | null;
      /** Source generation used to correlate playback events; independent of transport replacement generations. */
      generation: number;
    },
  ];
};

/** Session lifecycle events and native diagnostic notifications forwarded by the embedded engine. */
export interface RemediaLifecycleEvents extends SessionLifecycleEvents<unknown> {
  /** Emitted for a structured native diagnostic event. */
  [RemediaEvents.debug]: [import('./types.js').DiagnosticEvent];
}

/**
 * Local metadata and cancellation options for one playback generation; session configuration persists across generations.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export interface PlayOptions<Metadata = unknown> {
  /** Application-owned metadata retained locally for the source generation and attached to its events. */
  customMetadata?: Metadata;
  /**
   * Optional cancellation signal; cancellation before startup prevents playback, and cancellation after startup stops that generation.
   */
  signal?: AbortSignal;
}

/**
 * Caller-owned media input, metadata, and cancellation signal supplied to each awaited pre-play hook.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export interface BeforePlayContext<Metadata = unknown> {
  /** Caller-owned encoded input that the hooks inspect before the decoder opens it. */
  readonly source: MediaSource;
  /** Application-owned metadata retained locally for the source generation and attached to its events. */
  readonly customMetadata: Metadata | undefined;
  /** Cancellation signal for this hook invocation; check it before performing asynchronous work. */
  readonly signal: AbortSignal;
}

/**
 * Hook result: return nothing to continue, or cancel with an optional explanation to reject playback with AbortError.
 */
export type BeforePlayResult = void | {
  /** Return true from a pre-play hook to reject playback before opening the source. */
  cancel: true;
  /** Optional explanation for cancellation or a terminal playback event. */
  reason?: string;
};
/**
 * Awaited local callback run before a source opens; rejection or cancellation prevents that source from starting.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 * @param context - Source, local metadata, and cancellation signal supplied to the awaited hooks.
 * @returns Nothing to continue, or a cancellation result; a promise is awaited before source opening.
 */
export type BeforePlayHook<Metadata = unknown> = (
  context: BeforePlayContext<Metadata>,
) => BeforePlayResult | Promise<BeforePlayResult>;

/**
 * Ordered, awaited local middleware. It is not a server authorization mechanism.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export class PlaybackHooks<Metadata = unknown> {
  readonly #beforePlay: BeforePlayHook<Metadata>[] = [];

  /**
   * Append an awaited pre-play hook. Hooks run in registration order before a source opens.
   * @param hook - Local callback to await before opening each source; returning cancel rejects playback.
   * @returns An idempotent function that removes this hook registration.
   */
  beforePlay(hook: BeforePlayHook<Metadata>): () => void {
    if (typeof hook !== 'function') throw new TypeError('beforePlay hook must be a function');
    this.#beforePlay.push(hook);
    return () => {
      const index = this.#beforePlay.indexOf(hook);
      if (index >= 0) this.#beforePlay.splice(index, 1);
    };
  }

  /**
   * Run registered pre-play hooks in order and reject on cancellation or hook failure.
   * @param context - Source, local metadata, and cancellation signal supplied to the awaited hooks.
   * @returns Resolves when every hook permits playback; cancellation rejects with AbortError.
   */
  async runBeforePlay(context: BeforePlayContext<Metadata>): Promise<void> {
    for (const hook of [...this.#beforePlay]) {
      if (context.signal.aborted) throw abortError(context.signal.reason);
      const result = await raceAbort(
        Promise.resolve().then(() => hook(context)),
        context.signal,
      );
      if (context.signal.aborted) throw abortError(context.signal.reason);
      if (result?.cancel) {
        const error = new Error(result.reason ?? 'playback was cancelled by a beforePlay hook');
        error.name = 'AbortError';
        throw error;
      }
    }
  }

  /** @internal Release application hook references when the owning session is disposed. */
  clear(): void {
    this.#beforePlay.length = 0;
  }
}

/**
 * Create playback event correlation fields for one local source generation.
 * @param generation - Native source generation associated with the lifecycle notification.
 * @param customMetadata - Application-owned metadata associated with this source generation.
 * @returns Identity fields with metadata included only when provided.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 * @param sessionId - Opaque Remedia session identity used for event correlation.
 */
export function lifecycleContext<Metadata>(
  sessionId: string,
  generation: number,
  customMetadata: Metadata | undefined,
): PlaybackEventIdentity<Metadata> {
  return customMetadata === undefined
    ? { sessionId, nodeId: null, generation }
    : { sessionId, nodeId: null, generation, customMetadata };
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
