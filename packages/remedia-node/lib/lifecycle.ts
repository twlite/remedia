import type { MediaSource } from './input.js';
import type { AudioStreamInfo, PlaybackPosition, PlaybackState } from './types.js';

/** Stable names for playback/session event subscriptions and emission. */
export const PlaybackSessionEvents = {
  playbackStarted: 'playbackStarted',
  playbackCompleted: 'playbackCompleted',
  playbackStopped: 'playbackStopped',
  playbackFailed: 'playbackFailed',
  playbackPaused: 'playbackPaused',
  playbackResumed: 'playbackResumed',
  playbackSeeked: 'playbackSeeked',
  filterChanged: 'filterChanged',
  volumeChanged: 'volumeChanged',
  sessionDisposed: 'sessionDisposed',
} as const;

export type PlaybackSessionEvents =
  (typeof PlaybackSessionEvents)[keyof typeof PlaybackSessionEvents];

export const VoiceSessionEvents = {
  voiceStateChanged: 'voiceStateChanged',
  voiceFailed: 'voiceFailed',
} as const;

export type VoiceSessionEvents = (typeof VoiceSessionEvents)[keyof typeof VoiceSessionEvents];

export const RemediaEvents = {
  ...PlaybackSessionEvents,
  ...VoiceSessionEvents,
  debug: 'debug',
} as const;

export type RemediaEvents = (typeof RemediaEvents)[keyof typeof RemediaEvents];

export interface PlaybackEventIdentity<Metadata = unknown> {
  sessionId: string;
  guildId?: string;
  nodeId: string | null;
  generation: number;
  customMetadata?: Metadata;
  /** True only when a terminal event was reconstructed from an authoritative remote snapshot. */
  reconstructed?: boolean;
}

export type PlaybackLifecycleEvents<Metadata = unknown> = {
  [PlaybackSessionEvents.playbackStarted]: [
    PlaybackEventIdentity<Metadata> & { state: 'playing'; info: AudioStreamInfo },
  ];
  [PlaybackSessionEvents.playbackCompleted]: [
    PlaybackEventIdentity<Metadata> & { state: 'completed'; position: PlaybackPosition },
  ];
  [PlaybackSessionEvents.playbackStopped]: [
    PlaybackEventIdentity<Metadata> & { state: 'stopped'; reason?: string },
  ];
  [PlaybackSessionEvents.playbackFailed]: [
    PlaybackEventIdentity<Metadata> & { state: 'failed'; error: Error },
  ];
  [PlaybackSessionEvents.playbackPaused]: [
    PlaybackEventIdentity<Metadata> & { previousState: PlaybackState; state: 'paused' },
  ];
  [PlaybackSessionEvents.playbackResumed]: [
    PlaybackEventIdentity<Metadata> & { previousState: PlaybackState; state: 'playing' },
  ];
  [PlaybackSessionEvents.playbackSeeked]: [
    PlaybackEventIdentity<Metadata> & { positionMs: number; previousPositionMs: number | null },
  ];
  [PlaybackSessionEvents.filterChanged]: [
    PlaybackEventIdentity<Metadata> & { previousGraph: string | null; graph: string | null },
  ];
  [PlaybackSessionEvents.volumeChanged]: [
    PlaybackEventIdentity<Metadata> & { previousVolume: number; volume: number },
  ];
};

export interface VoiceEventIdentity {
  sessionId: string;
  guildId: string;
  nodeId: string | null;
}

export type LifecycleVoiceState =
  | 'connecting'
  | 'negotiating_dave'
  | 'connected'
  | 'reconnecting'
  | 'disconnecting'
  | 'disconnected'
  | 'failed';

export type VoiceLifecycleEvents = {
  [VoiceSessionEvents.voiceStateChanged]: [
    VoiceEventIdentity & { previousState: LifecycleVoiceState; state: LifecycleVoiceState },
  ];
  [VoiceSessionEvents.voiceFailed]: [VoiceEventIdentity & { state: 'failed'; error: Error }];
};

export type SessionLifecycleEvents<Metadata = unknown> = PlaybackLifecycleEvents<Metadata> &
  VoiceLifecycleEvents & {
    [PlaybackSessionEvents.sessionDisposed]: [
      { sessionId: string; guildId?: string; nodeId: string | null; generation: number },
    ];
  };

export type PlaybackSessionEventMap<Metadata = unknown> = PlaybackLifecycleEvents<Metadata> & {
  [PlaybackSessionEvents.sessionDisposed]: [
    { sessionId: string; guildId?: string; nodeId: string | null; generation: number },
  ];
};

export interface RemediaLifecycleEvents extends SessionLifecycleEvents<unknown> {
  [RemediaEvents.debug]: [import('./types.js').DiagnosticEvent];
}

export interface PlayOptions<Metadata = unknown> {
  customMetadata?: Metadata;
  /** Aborting before startup cancels hooks; aborting after startup stops this playback. */
  signal?: AbortSignal;
}

export interface BeforePlayContext<Metadata = unknown> {
  readonly source: MediaSource;
  readonly customMetadata: Metadata | undefined;
  /** Aborted when the operation signal fires or the session is disposed. */
  readonly signal: AbortSignal;
}

export type BeforePlayResult = void | { cancel: true; reason?: string };
export type BeforePlayHook<Metadata = unknown> = (
  context: BeforePlayContext<Metadata>,
) => BeforePlayResult | Promise<BeforePlayResult>;

/** Ordered, awaited local middleware. It is not a server authorization mechanism. */
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
