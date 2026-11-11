import type { EventEmitter } from 'node:events';
import type {
  AudioStreamInfo,
  DiscordSessionOptions,
  GuildAssignment,
  MediaSource,
  NodeSnapshot,
  NodeStatus,
  NodeType,
  PlaybackCompletion,
  PlaybackPosition,
  PlaybackSnapshot,
  PlaybackState,
  PlaybackLifecycleEventMap,
  PlayOptions,
  StageNextOptions,
  StageSnapshot,
  VoiceConnectionState,
  VoiceLifecycleEventMap,
} from './types.js';

/** Playback and voice lifecycle notifications emitted by an assigned backend. */
export type GuildBackendEvents = PlaybackLifecycleEventMap<unknown> & VoiceLifecycleEventMap;

/**
 * Internal guild player contract shared by embedded and remote execution; transport and playback generations remain independent.
 */
export interface GuildBackend extends EventEmitter<GuildBackendEvents> {
  /** Configured identifier of the Remedia node associated with this resource. */
  readonly nodeId: string;
  /** Execution mode of the assigned backend, or null before assignment. */
  readonly nodeType: NodeType;
  /** Current playback lifecycle state. */
  readonly state: PlaybackState;
  /** Latest source and output timeline measurements in milliseconds. */
  readonly position: PlaybackPosition;
  /** Locally estimated output elapsed time in ms, reconciled with authoritative native snapshots. */
  getPlaybackTime(): number;
  /** Source duration in milliseconds, or null when unknown. */
  readonly durationMs: number | null;
  /** Most recent playback or voice failure message, or null when no error is available. */
  readonly error: string | null;
  /** Session master linear gain from 0 through 2, independent of filtergraph processing; defaults to 1. */
  readonly volume: number;
  /** Trusted raw FFmpeg audio filtergraph, or null to disable filtering; defaults to null. */
  readonly filterGraph: string | null;
  /** Current Discord transport lifecycle state, independent of playback pause or completion. */
  readonly connectionState: VoiceConnectionState;
  /** Latest known playback snapshot, including persistent configuration and source generation. */
  readonly snapshot: PlaybackSnapshot;
  /**
   * Request fresh Gateway credentials and replace only the voice transport, retaining playback and pause state. A newer move supersedes a pending destination.
   * @param channelId - Destination Discord voice channel snowflake as a positive decimal string.
   * @returns Resolves when the requested destination connects; rejects on supersession, cancellation, or connection failure.
   */
  move?(channelId: string): Promise<void>;
  /**
   * Update Discord self-deaf state without rebuilding playback. Unchanged values send nothing.
   * @param enabled - The requested Discord self-state flag.
   * @returns Resolves after storing the flag or sending its Gateway update; rejects if the update fails.
   */
  setSelfDeaf?(enabled: boolean): Promise<void>;
  /**
   * Update Discord self-mute state without changing audio gain or playback pause. Unchanged values send nothing.
   * @param enabled - The requested Discord self-state flag.
   * @returns Resolves after storing the flag or sending its Gateway update; rejects if the update fails.
   */
  setSelfMute?(enabled: boolean): Promise<void>;
  /**
   * Join the requested voice destination and wait for Gateway credentials and transport readiness. Concurrent calls share the pending attempt.
   * @param options - Optional cancellation signal and positive timeout in milliseconds; defaults to 100000.
   * @returns Resolves when connected; rejects on timeout, cancellation, session loss, or transport failure.
   */
  connect(options?: {
    /**
     * Optional cancellation signal; cancellation before startup prevents playback, and cancellation after startup stops that generation.
     */
    signal?: AbortSignal;
    /** Maximum connection negotiation time in milliseconds; defaults to 100000. */
    timeoutMs?: number;
  }): Promise<void>;
  /**
   * Install fresh bot Gateway credentials under a transport generation fence, retaining the guild playback session.
   * @param credentials - Fresh Discord voice session ID, token, endpoint, and optional replacement channel.
   * @param transportGeneration - Monotonic voice operation generation used to reject stale replacements and cleanup.
   * @returns Resolves when transport setup succeeds; rejects stale generations or failed connections.
   */
  connectWithCredentials?(
    credentials: {
      /** Discord voice channel snowflake as a decimal string; the requested mutable destination. */
      channelId?: string;
      /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
      sessionId: string;
      /** Fresh Discord voice credential token obtained from the bot Gateway; keep it private. */
      token: string;
      /** Discord voice server endpoint obtained from the bot Gateway. */
      endpoint: string;
    },
    transportGeneration?: number,
  ): Promise<void>;
  /**
   * Park playback output and detach the old voice transport for a replacement generation.
   * @param transportGeneration - Monotonic voice operation generation used to reject stale replacements and cleanup.
   * @returns Resolves after transport suspension; the source and decoder remain allocated.
   */
  suspendVoiceRecovery?(transportGeneration?: number): Promise<void>;
  /**
   * Terminate parked voice recovery after its retry budget is exhausted.
   * @param transportGeneration - Monotonic voice operation generation used to reject stale replacements and cleanup.
   * @returns Resolves after the backend marks recovery failed.
   */
  failVoiceRecovery?(transportGeneration?: number): Promise<void>;
  /**
   * Reject a bot user that differs from the identity bound to this remote connection.
   * @param userId - Discord bot user snowflake; distinct from Remedia client session identity.
   */
  validateIdentity?(userId: string): void;
  /**
   * Cancel connection attempts, leave Gateway voice, and close the transport and active playback. The wrapper can reconnect afterward.
   * @param transportGeneration - Monotonic voice operation generation used to reject stale replacements and cleanup.
   * @returns Resolves after cleanup; concurrent disconnects share cleanup work.
   */
  disconnect(transportGeneration?: number): Promise<void>;
  /**
   * Disconnect and unregister this session, releasing its owned resources. Repeated calls share disposal work.
   * @returns Resolves after disposal; rejects if resource cleanup fails.
   */
  dispose(): Promise<void>;
  /**
   * Open one media source and begin a new playback generation using the persistent session configuration. Stop active playback before starting another source.
   * @param source - Encoded media input; Buffer and caller-owned binary Readable inputs require embedded mode.
   * @param options - Optional metadata for this generation and a cancellation signal.
   * @returns Selected stream metadata once startup succeeds; rejects if hooks, input opening, or playback startup fail.
   */
  stageNext(source: MediaSource, options: StageNextOptions<unknown>): Promise<StageSnapshot>;
  clearStage(): Promise<void>;
  stagedSource(): Promise<StageSnapshot | null>;
  play(
    source: MediaSource,
    options?: PlayOptions<unknown>,
  ): Promise<{
    /** Metadata for the audio stream that started playback. */
    info: AudioStreamInfo;
    /** Source generation used to correlate playback events; independent of transport replacement generations. */
    generation: number;
  }>;
  /**
   * Pause the current playback without replacing its decoder or source generation.
   * @returns Resolves when the backend accepts the pause request.
   */
  pause(): Promise<void>;
  /**
   * Resume paused playback on its existing source generation.
   * @returns Resolves when the backend accepts the resume request.
   */
  resume(): Promise<void>;
  /**
   * Seek on the original media timeline; non-seekable and one-shot sources may reject.
   * @param positionMs - Non-negative finite position on the original media timeline, in milliseconds.
   * @returns Resolves after the backend accepts the seek; rejects for invalid positions or unsupported inputs.
   */
  seek(positionMs: number): Promise<void>;
  /**
   * Stop the active playback generation and release its input without leaving the voice channel.
   * @returns Resolves after the backend stops the active source.
   */
  stop(): Promise<void>;
  /**
   * Wait for the current playback generation to reach a terminal state.
   * @returns Terminal state and the playback generation that completed.
   */
  wait(): Promise<PlaybackCompletion>;
  /**
   * Apply a trusted raw FFmpeg filtergraph, or disable filtering with null, without changing gain.
   * @param graph - Trusted, non-empty raw FFmpeg audio filtergraph; native validation errors reject the operation.
   * @returns Resolves after configuration is accepted; invalid graphs reject.
   */
  setFilterGraph(graph: string | null): Promise<void>;
  /**
   * Set persistent linear master gain independently of the configured filtergraph; unchanged gain emits no change event.
   * @param gain - Finite linear master gain from 0 through 2; 1 leaves amplitude unchanged.
   * @returns Resolves after the gain is stored or applied; rejects for non-finite gain or values outside 0 through 2.
   * @throws {RangeError} If gain is non-finite or outside the inclusive range 0 through 2.
   */
  setVolume(gain: number): Promise<void>;
  /**
   * Refresh the latest playback snapshot from its backend.
   * @returns The refreshed snapshot, or the retained initial snapshot before backend allocation.
   */
  refresh(): Promise<PlaybackSnapshot>;
  /**
   * Mark the remote session unavailable and fail pending operations for its retained assignment.
   * @param reason - Explanation for remote session loss or operation failure.
   */
  markSessionLost(reason: string): void;
}

/** Internal lifecycle, capacity, Gateway routing, and guild allocation contract for a configured node. */
export interface NodeAdapter {
  /**
   * Inspect disposable media without assigning a guild or creating playback.
   * @param source - Encoded media supported by this backend.
   * @param options - Cancellation and total deadline, defaulting to 15000 ms.
   * @returns Available stream metadata; rejects unavailable nodes, invalid inputs, timeout, or cancellation.
   */
  probe(source: MediaSource, options?: import('./types.js').ProbeOptions): Promise<AudioStreamInfo>;
  /** Application-configured node identifier, unique within the client node pool. */
  readonly id: string;
  /** Discriminant selecting embedded or remote node execution. */
  readonly type: NodeType;
  /** Current node availability or guild assignment status. */
  readonly status: NodeStatus;
  /** Latest known playback snapshot, including persistent configuration and source generation. */
  readonly snapshot: NodeSnapshot;
  /**
   * Initialize the node connection or embedded engine.
   * @returns Resolves when initialization succeeds; rejects if startup fails.
   */
  start(): Promise<void>;
  /**
   * Dispose owned sessions, close node resources, and release the client or engine.
   * @returns Resolves after cleanup; rejects when a resource fails to close.
   */
  close(): Promise<void>;
  /**
   * Forward a raw bot Gateway packet to the matching voice session; unrelated packets are ignored.
   * @param packet - Raw packet from this bot's Gateway dispatch stream.
   */
  receive(packet: unknown): void;
  /**
   * Register a backend wrapper for one guild on this node.
   * @param options - Guild identity and initial playback and voice configuration.
   * @param onDispose - Callback invoked when this wrapper releases its owning registration.
   * @returns The guild backend associated with this node.
   */
  createGuildSession(options: DiscordSessionOptions, onDispose: () => void): GuildBackend;
  /**
   * Release the node-owned backend and remote guild resources.
   * @param guildId - Discord guild snowflake used to route the operation.
   * @returns Resolves after guild cleanup finishes.
   */
  releaseGuild(guildId: string): Promise<void>;
}

/** Sticky guild assignment state exposed by the client. */
export type AssignmentStatus = GuildAssignment['status'];
