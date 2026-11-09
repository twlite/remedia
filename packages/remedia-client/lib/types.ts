import type { Readable } from 'node:stream';

/** Event names shared by embedded and remote playback-session adapters. */
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

/** Stable event names for typed RemediaClientEventNames subscriptions. */
export const RemediaClientEventNames = {
  /** Emitted when a configured node becomes healthy. */
  nodeConnected: 'nodeConnected',
  /** Emitted when a configured node disconnects or loses its remote session. */
  nodeDisconnected: 'nodeDisconnected',
  /** Emitted after a remote client session resumes and its guild snapshot is reconciled. */
  sessionResumed: 'sessionResumed',
  /** Emitted when a remote client session cannot be resumed. */
  sessionLost: 'sessionLost',
  /** Emitted for a sequenced lifecycle notification received from a remote server. */
  nodeEvent: 'nodeEvent',
  /** Emitted when a remote server reports dropped buffered events. */
  eventsLost: 'eventsLost',
  /** Emitted when sticky routing assigns a guild to a configured node. */
  guildAssigned: 'guildAssigned',
  /** Emitted when a guild assignment is released. */
  guildReleased: 'guildReleased',
  /** Emitted when a guild switches between configured nodes. */
  guildMigrated: 'guildMigrated',
} as const;

/** Union of the stable event names defined by RemediaClientEventNames. */
export type RemediaClientEventNames =
  (typeof RemediaClientEventNames)[keyof typeof RemediaClientEventNames];

/** Lifecycle state of a playback pipeline, independent of its voice transport state. */
export type PlaybackState =
  | 'idle'
  | 'starting'
  | 'playing'
  | 'pausing'
  | 'resuming'
  | 'paused'
  | 'stopping'
  | 'stopped'
  | 'completed'
  | 'failed';

/** Discord transport lifecycle state, including credential negotiation and recovery. */
export type VoiceConnectionState =
  | 'connecting'
  | 'negotiating_dave'
  | 'connected'
  | 'reconnecting'
  | 'disconnecting'
  | 'disconnected'
  | 'failed';

/** Metadata and format information for the selected audio stream; unavailable measurements are null. */
export interface AudioStreamInfo {
  /** Zero-based index of the selected audio stream in the media container. */
  streamIndex: number;
  /** Codec name reported by the selected audio stream. */
  codec: string;
  /** Encoded stream sample rate in Hz, or null when unknown; playback output is normalized separately. */
  sampleRate: number | null;
  /** Encoded stream channel count, or null when unknown. */
  channels: number | null;
  /** Encoded stream channel layout description, or null when unknown. */
  channelLayout: string | null;
  /** Source duration in milliseconds, or null when unknown. */
  durationMs: number | null;
  /** Stream bit rate in bits per second, or null when unknown. */
  bitRate: number | null;
  /** Container or stream metadata keyed by native tag name. */
  metadata: Readonly<Record<string, string>>;
}

/** Source and output timeline measurements in milliseconds; unsupported measurements are null. */
export interface PlaybackPosition {
  /** Position on the original source timeline in milliseconds, or null when unavailable. */
  mediaMs: number | null;
  /** Whether source position comes from a timestamp or an estimate; null when unavailable. */
  mediaPositionAccuracy: 'timestamp' | 'estimated' | null;
  /**
   * Duration of audio submitted to the output in milliseconds; may differ from source position after filtering.
   */
  submittedMs: number;
  /** Output-reported played duration in milliseconds, or null when the output does not provide it. */
  playedMs: number | null;
}

/** Latest known playback state and persistent configuration for one guild player. */
export interface PlaybackSnapshot {
  /** Current playback lifecycle state. */
  state: PlaybackState;
  /** Source generation used to correlate playback events; independent of transport replacement generations. */
  generation: number;
  /** Active passthrough or PCM transcoding path, or null before a processing path is known. */
  processingMode: 'passthrough' | 'transcoding' | null;
  /** Latest source and output timeline measurements in milliseconds. */
  position: PlaybackPosition;
  /** Source duration in milliseconds, or null when unknown. */
  durationMs: number | null;
  /** Selected source information, or null before source metadata is available. */
  source: AudioStreamInfo | null;
  /** Trusted raw FFmpeg audio filtergraph, or null to disable filtering; defaults to null. */
  filterGraph: string | null;
  /** Session master linear gain from 0 through 2, independent of filtergraph processing; defaults to 1. */
  volume: number;
  /** Most recent structured playback failure, or null when none is recorded. */
  /** Optional crossfade policy and staged state reported by capable nodes. */
  crossfade?: {
    enabled: true;
    durationMs: number;
    prepareBeforeMs: number;
    curve: 'equalPower';
  } | null;
  staged?: StageSnapshot | null;
  lastError: {
    /** Machine-readable error code supplied by the remote server. */
    code: string;
    /** Human-readable diagnostic or failure description. */
    message: string;
  } | null;
}

/** JSON-compatible value. Plugin operation input is opaque to Remedia and must be JSON-serializable. */
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };

/**
 * A media source produced on demand by a generic plugin-host operation.
 *
 * Remedia never interprets `input`; the consuming application owns its schema. The selected
 * remote node executes the operation on the configured host only when playback or staging
 * prepares the source, and the resulting temporary media location is never stored as the
 * source identity. Operation sources are remote-only, cannot be probed, and are not
 * migrated to another node: host affinity is explicit, with no fallback to another deployment.
 */
export interface OperationMediaSource {
  /** Operator-assigned ID of a plugin-host deployment in the node's `pluginHosts` configuration. */
  hostId: string;
  /** Plugin registration ID within that deployment. */
  pluginId: string;
  /** Operation name which the node's configuration allows for this host and plugin. */
  operation: string;
  /** Opaque JSON passed unchanged to the operation. Its serialized size is bounded by the host's `maxRequestBytes`. */
  input: JsonValue;
}

/**
 * Encoded media input. Files, HTTP(S) URLs and plugin operations work in remote mode; files and URLs also work embedded;
 * owned bytes and caller-owned binary Readables require embedded playback.
 */
export type MediaSource =
  | {
      /** Local media file path; remote mode resolves it on the server and enforces configured file roots. */
      file: string;
    }
  | {
      /** HTTP(S) media URL opened by the selected engine; remote nodes enforce server URL policy. */
      url: string;
    }
  | OperationMediaSource
  | {
      /** Owned encoded media bytes, supported by embedded nodes only. */
      buffer: Buffer | Uint8Array;
    }
  | {
      /**
       * Caller-owned binary Readable, consumed with bounded backpressure in embedded mode; Remedia does not destroy it.
       */
      stream: Readable;
    };

/**
 * Media resolved by the server: an allowed server-local file, an HTTP(S) URL, or a plugin-host operation.
 * Client byte uploads are unsupported.
 */
export type RemoteMediaSource =
  | {
      /** Local media file path; remote mode resolves it on the server and enforces configured file roots. */
      file: string;
    }
  | {
      /** HTTP(S) media URL opened by the selected engine; remote nodes enforce server URL policy. */
      url: string;
    }
  | OperationMediaSource;

/** Execution mode of a configured Remedia node. */
export type NodeType = 'embedded' | 'remote';
/** Client-observed availability of a node and its resumable client session. */
export type NodeStatus = 'connecting' | 'healthy' | 'disconnected' | 'session_lost' | 'closed';

/** Native process memory accounting in bytes. Missing fields indicate an older server; null indicates unavailable data. */
export interface ResourceMemory {
  /** Resident bytes including shared pages. */
  residentBytes?: number | null;
  /** Private resident bytes (USS); excludes shared pages, distinct from private commit. */
  privateResidentBytes?: number | null;
  /** Proportional resident bytes (PSS), including shared pages proportionally. */
  proportionalResidentBytes?: number | null;
  /** Private committed bytes, including potentially nonresident pages. */
  committedPrivateBytes?: number | null;
  /** Mapped virtual address bytes; not physical RAM. */
  virtualBytes?: number | null;
  /** Reserved but uncommitted virtual address bytes; not RAM. */
  reservedVirtualBytes?: number | null;
  /** Lifetime peak resident bytes. */
  peakResidentBytes?: number | null;
  /** macOS physical footprint bytes; distinct from USS and RSS. */
  footprintBytes?: number | null;
}

/** Native process CPU accounting. Missing fields indicate an older server; null indicates unavailable data. */
export interface ResourceCpu {
  /** Process utilization: 100 means one logical CPU; can exceed 100. Null on the first sample. */
  processPercent?: number | null;
  /** Cumulative process user CPU time in milliseconds. */
  userTimeMs?: number | null;
  /** Cumulative process system CPU time in milliseconds. */
  systemTimeMs?: number | null;
  /** Available logical CPU parallelism estimate. */
  availableParallelism?: number | null;
  /** Actual process OS thread count, including nonaudio threads. */
  threadCount?: number | null;
  /** System one-minute load average; not process CPU usage. */
  loadAverage1m?: number | null;
}

/** Cached native measurements for the whole host process, shared by all embedded instances. */
export interface ProcessResourceStatistics {
  /** Process memory counters in bytes. */
  memory: ResourceMemory;
  /** Process CPU and system load counters. */
  cpu: ResourceCpu;
  /** Collection completion timestamp as Unix milliseconds; null before the first collection. */
  collectedAtUnixMs: number | null;
  /** Wall interval used for process utilization in milliseconds; null on the first sample. */
  cpuSampleIntervalMs: number | null;
  /** Demand-driven cache refresh interval in milliseconds; concurrent requests share stale snapshots. */
  samplingIntervalMs: number;
  /** Native collection duration in milliseconds, or null before collection. */
  collectionDurationMs: number | null;
  /** Unix descriptors or Windows handles owned by the process, or null if unavailable. */
  openHandles: number | null;
}

/** Latest capacity, transport, processing, CPU, and memory statistics reported by a node. */
export interface NodeStatistics {
  /** Number of currently allocated players on the node. */
  playersActive: number;
  /** Maximum number of players allowed by the node. */
  playersMaximum: number;
  /** Available player slots from the current allocation snapshot, absent on older clients. */
  playersAvailable?: number;
  /** Sources in active lifecycle states, including paused sources; null on older servers. */
  playbackActive?: number | null;
  /** Number of currently open remote connections. */
  connectionsActive: number;
  /** Maximum number of remote connections allowed by the node. */
  connectionsMaximum: number;
  /** Node-reported transport health status. */
  transportHealth: string;
  /** Node-reported audio processing health status. */
  processingHealth: string;
  /** Node-reported CPU measurements; unsupported measurements are null. */
  cpu: Readonly<ResourceCpu>;
  /** Node-reported memory measurements; unsupported measurements are null. */
  memory: Readonly<ResourceMemory>;
  /** Cached process-wide resources, absent on older servers. */
  resources?: ProcessResourceStatistics | null;
}

/** Advertised crossfade policy. An enabled server setting is not sufficient:
 * available becomes true only after the native mixer and promotion protocol work.
 */
export interface CrossfadeCapability {
  /** Whether node policy permits clients to request crossfade. */
  configured: boolean;
  /** Whether this node implements crossfade and policy enables it. */
  available: boolean;
  /** Overlap duration in milliseconds; defaults to 5000. */
  durationMs: number;
  /** Preparation lead before overlap begins, in milliseconds; defaults to 30000. */
  prepareBeforeMs: number;
  /** Equal-power mixing curve; the only supported curve. */
  curve: 'equalPower';
}

/** Plugin-host operation sources supported by a node. Older nodes omit it, which means unsupported. */
export interface PluginOperationsCapability {
  /** Whether the node can execute operation sources at all. */
  available: boolean;
  /** Configured plugin-host IDs the node can address. */
  hosts: readonly string[];
}

/** Capabilities advertised by a node's `node.info` response. */
export interface NodeCapabilities {
  /** Crossfade policy and implementation capability. */
  crossfade: CrossfadeCapability;
  /** Plugin-host operation support. Absent capabilities are treated as unavailable. */
  pluginOperations: PluginOperationsCapability;
}

/** Opt-in request for staged-source crossfading on an individual guild session. */
export interface SessionCrossfadeOptions {
  /** Explicit session opt-in; omitted sessions retain ordinary playback. */
  enabled: true;
  /** Duration of overlapping audio, 1000-30000 milliseconds. */
  durationMs?: number;
  /** Lead time before the overlap starts, 5000-120000 milliseconds. */
  prepareBeforeMs?: number;
  /** Equal-power mixing curve; the only supported curve. */
  curve?: 'equalPower';
}

/** Read-only client view of a node, its health, capacity, and assigned guilds. */
export interface NodeSnapshot {
  /** Application-configured node identifier, unique within the client node pool. */
  id: string;
  /** Discriminant selecting embedded or remote node execution. */
  type: NodeType;
  /** Current node availability or guild assignment status. */
  status: NodeStatus;
  /** Runtime server identity reported by the remote node, or null before discovery or for embedded nodes. */
  nodeId: string | null;
  /** Server software version, or null before it is reported. */
  version: string | null;
  /** Negotiated RPC protocol version, or null for an uninitialized or embedded node. */
  protocolVersion: number | null;
  /** Capabilities advertised by the backend; absent for older servers. */
  capabilities?: NodeCapabilities;
  /** Latest node statistics, or null before a report is available. */
  statistics: NodeStatistics | null;
  /** Maximum simultaneous guild assignments considered by client-side node selection. */
  capacity: number;
  /** Guild IDs currently assigned to this node by the client. */
  assignedGuilds: readonly string[];
  /** Latest node connection or protocol failure message, or null when none is recorded. */
  lastError: string | null;
}

/** HTTP Basic authentication credentials for a remote node; keep these values private. */
export interface RemoteNodeAuth {
  /** Remote HTTP Basic authentication username. */
  username: string;
  /** Remote HTTP Basic authentication password; do not log this value. */
  password: string;
}

/** Configuration for an in-process native engine; requires the optional @remedia/node package. */
export interface EmbeddedNodeConfig {
  /** Discriminant selecting embedded or remote node execution. */
  type: 'embedded';
  /** Application-configured node identifier, unique within the client node pool. */
  id: string;
  /** Maximum simultaneous guild sessions used by client-side selection. Defaults to 64. */
  capacity?: number;
}

/** Connection and capacity configuration for one standalone WebSocket server. */
export interface RemoteNodeConfig {
  /** Discriminant selecting embedded or remote node execution. */
  type: 'remote';
  /** Application-configured node identifier, unique within the client node pool. */
  id: string;
  /** Remote node endpoint using remedia, remedias, ws, or wss scheme. */
  url: string;
  /** Explicit auth takes precedence over URL userinfo. */
  auth?: RemoteNodeAuth;
  /** Overrides the server's reported player maximum for client-side selection. */
  capacity?: number;
}

/** Configuration for either an embedded native engine or a remote server. */
export type RemediaNodeConfig = EmbeddedNodeConfig | RemoteNodeConfig;

/** Discord Gateway OP 4 payload sent through the application-owned bot connection. */
export interface DiscordGatewayVoicePayload {
  /** Discord Gateway operation code; voice state requests use 4. */
  op: 4;
  /** Gateway payload data; voice requests contain guild, channel, and self-state fields. */
  d: {
    /** Discord guild snowflake to route through the bot Gateway. */
    guild_id: string;
    /** Voice channel snowflake, or null to leave voice. */
    channel_id: string | null;
    /** Discord self-mute flag; independent of playback volume and pause. */
    self_mute: boolean;
    /** Discord self-deaf flag; independent of playback volume and pause. */
    self_deaf: boolean;
  };
}

/**
 * Application callback that routes an OP 4 payload to the shard responsible for a guild.
 * @param guildId - Discord guild snowflake used to route the operation.
 * @param payload - Discord Gateway OP 4 payload to send through the guild shard.
 */
export type DiscordGatewaySend = (
  guildId: string,
  payload: DiscordGatewayVoicePayload,
) => void | Promise<void>;

/** Raw Discord Gateway packet forwarded by the host application; unrelated dispatches are ignored. */
export interface DiscordGatewayPacket {
  /** Gateway dispatch event name, when the packet represents a dispatch. */
  t?: string;
  /** Gateway dispatch sequence used to reject stale events, when supplied by the host. */
  s?: number;
  /** Raw event-specific Gateway payload, validated only for supported voice dispatches. */
  d?: unknown;
  /**
   * Additional Gateway packet fields forwarded unchanged by the host application.
   * @param key - Additional Gateway packet field name.
   */
  [key: string]: unknown;
}

/**
 * Identity, initial playback configuration, and Gateway voice flags for one guild session. The channel is a mutable destination, while userId identifies the bot.
 */
export interface DiscordSessionOptions {
  /** Discord Gateway self-deaf state; defaults to true. */
  selfDeaf?: boolean;
  /** Discord Gateway self-mute state; defaults to false. */
  selfMute?: boolean;
  /** Session master linear gain, finite from 0 through 2; defaults to 1. */
  volume?: number;
  /** Trusted raw FFmpeg audio filtergraph; defaults to null. Native syntax validation occurs during backend configuration before playback. */
  filterGraph?: string | null;
  /** Optional staged-source crossfade request, disabled when omitted. */
  crossfade?: SessionCrossfadeOptions | false;
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  guildId: string;
  /** Discord voice channel snowflake as a decimal string; the requested mutable destination. */
  channelId: string;
  /** Discord bot user snowflake as a decimal string; separate from Remedia session identity. */
  userId: string;
  /** Configured identifier of the Remedia node associated with this resource. */
  nodeId?: string;
  /**
   * Plugin-host IDs which the assigned node must be configured with. Automatic assignment only
   * considers nodes whose `pluginOperations` capability lists every ID; a pinned `nodeId` that lacks one
   * is rejected. Without this option, assignment ignores plugin hosts and operation sources fail explicitly
   * if the assigned node cannot execute them.
   */
  pluginHosts?: readonly string[];
}

/**
 * Local metadata and cancellation options for one playback generation; session configuration persists across generations.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export interface PlayOptions<Metadata = unknown> {
  /** Arbitrary application-owned value retained in this process and attached to one generation. */
  customMetadata?: Metadata;
  /**
   * Optional cancellation signal; cancellation before startup prevents playback, and cancellation after startup stops that generation.
   */
  signal?: AbortSignal;
}

/**
 * Correlation fields identifying the session and source generation that produced a playback event.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */

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
  guildId: string;
  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  nodeId: string | null;
  /** Source generation used to correlate playback events; independent of transport replacement generations. */
  generation: number;
  /** Application-owned metadata retained locally for the source generation and attached to its events. */
  customMetadata?: Metadata;
  /** True when a terminal notification was reconstructed from an authoritative remote snapshot. */
  reconstructed?: boolean;
}

/**
 * Typed argument tuples for playback events. Terminal events refer to their originating source generation.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export type PlaybackLifecycleEventMap<Metadata = unknown> = {
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
  /** Configured node identifier associated with this resource; null when no node has been assigned. */
  nodeId: string | null;
}

/** Typed argument tuples for voice transport state changes and failures. */
export type VoiceLifecycleEventMap = {
  /** Emitted when the Discord transport lifecycle state changes. */
  [VoiceSessionEvents.voiceStateChanged]: [
    VoiceEventIdentity & {
      /** Playback or transport lifecycle state before this transition. */
      previousState: VoiceConnectionState;
      /** Current playback lifecycle state. */
      state: VoiceConnectionState;
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
 * Combined playback, voice, and disposal events emitted by a Discord session.
 * @typeParam Metadata - Application-owned metadata attached to playback generations.
 */
export type DiscordSessionEvents<Metadata = unknown> = PlaybackLifecycleEventMap<Metadata> &
  VoiceLifecycleEventMap & {
    /** Emitted when a session is unregistered and its owned resources are released. */
    [PlaybackSessionEvents.sessionDisposed]: [
      VoiceEventIdentity & {
        /** Source generation used to correlate playback events; independent of transport replacement generations. */
        generation: number;
      },
    ];
  };

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

/** Cancellation and timeout controls for obtaining credentials and establishing a voice transport. */
export interface DiscordConnectOptions {
  /** Optional signal that cancels Gateway negotiation and pending transport setup. */
  signal?: AbortSignal;
  /** Maximum connection negotiation time in milliseconds; defaults to 100000. */
  timeoutMs?: number;
}

/** Read-only identity, availability, statistics, and assignments for a configured node. */
export interface RemoteNodeView {
  /** Application-configured node identifier, unique within the client node pool. */
  readonly id: string;
  /** Discriminant selecting embedded or remote node execution. */
  readonly type: NodeType;
  /** Current node availability or guild assignment status. */
  readonly status: NodeStatus;
  /** Runtime server identity reported by the remote node, or null before discovery or for embedded nodes. */
  readonly nodeId: string | null;
  /** Latest node statistics, or null before a report is available. */
  readonly statistics: NodeStatistics | null;
  /** Guild IDs currently assigned to this node by the client. */
  readonly assignedGuilds: readonly string[];
}

/**
 * Application selection callback that chooses a configured node ID from the eligible healthy nodes.
 * @param nodes - Eligible healthy node snapshots with available capacity.
 * @param context - Guild being assigned and its source, when available.
 * @returns Configured identifier of an eligible node, directly or through a promise.
 */
export type NodeSelection = (
  nodes: readonly NodeSnapshot[],
  context: {
    /** Discord guild snowflake as a decimal string; unique within the owning client. */
    guildId: string;
    /** Selected source information, or null before source metadata is available. */
    source?: MediaSource;
  },
) => string | Promise<string>;

/** Node pool, bot Gateway integration, and bounded remote-request settings for a client. */
export interface RemediaClientOptions {
  /** Configured embedded and remote nodes; at least one node and unique configured IDs are required. */
  nodes: readonly RemediaNodeConfig[];
  /** Application Gateway callback that sends OP 4 through the shard responsible for a guild. */
  send: DiscordGatewaySend;
  /** Discord bot snowflake sent as `X-Remedia-Bot-Id` on remote WebSocket handshakes. */
  botUserId?: string;
  /** Bounded WebSocket RPC timeout. Defaults to 15 seconds. */
  requestTimeoutMs?: number;
  /** Maximum concurrent RPC requests per remote node. Defaults to 128. */
  maxPendingRequests?: number;
  /** Optional override for deterministic or application-specific capacity selection. */
  selectNode?: NodeSelection;
}

/** Sticky routing of one guild to a configured node; lost assignments remain until disposal. */
export interface GuildAssignment {
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  guildId: string;
  /** Configured identifier of the Remedia node associated with this resource. */
  nodeId: string;
  /** Execution mode of the assigned backend, or null before assignment. */
  nodeType: NodeType;
  /** Current node availability or guild assignment status. */
  status: 'assigned' | 'lost';
}

/** Sequenced server notification scoped to a remote client session. */
export interface ServerLifecycleEvent {
  /** Configured identifier of the Remedia node associated with this resource. */
  nodeId: string;
  /** Server or diagnostic event name. */
  event: string;
  /** Opaque Remedia session identity used to correlate events; distinct from the Discord bot user ID. */
  sessionId: string;
  /** Monotonic server event sequence used to detect missed or stale notifications. */
  sequence: number;
  /** Event-specific structured payload. */
  data: Readonly<Record<string, unknown>>;
}

/**
 * Session and node lifecycle events forwarded by the client, including assignment and resumption notifications.
 */
export interface RemediaClientEvents
  extends PlaybackLifecycleEventMap<unknown>, VoiceLifecycleEventMap {
  /** Emitted when a configured node becomes healthy. */
  [RemediaClientEventNames.nodeConnected]: [NodeSnapshot];
  /** Emitted when a configured node disconnects or loses its remote session. */
  [RemediaClientEventNames.nodeDisconnected]: [
    {
      /** Snapshot of the node associated with this notification. */
      node: NodeSnapshot;
      /** Most recent playback or voice failure message, or null when no error is available. */
      error: Error | null;
    },
  ];
  /** Emitted after a remote client session resumes and its guild snapshot is reconciled. */
  [RemediaClientEventNames.sessionResumed]: [
    {
      /** Snapshot of the node associated with this notification. */
      node: NodeSnapshot;
      /** Guild snowflakes affected by the lost or resumed remote client session. */
      guildIds: readonly string[];
    },
  ];
  /** Emitted when a remote client session cannot be resumed. */
  [RemediaClientEventNames.sessionLost]: [
    {
      /** Snapshot of the node associated with this notification. */
      node: NodeSnapshot;
      /** Guild snowflakes affected by the lost or resumed remote client session. */
      guildIds: readonly string[];
      /** Optional explanation for cancellation or a terminal playback event. */
      reason: string;
    },
  ];
  /** Emitted for a sequenced lifecycle notification received from a remote server. */
  [RemediaClientEventNames.nodeEvent]: [ServerLifecycleEvent];
  /** Emitted when a remote server reports dropped buffered events. */
  [RemediaClientEventNames.eventsLost]: [
    {
      /** Configured identifier of the Remedia node associated with this resource. */
      nodeId: string;
      /** Number of server notifications omitted from the bounded event buffer. */
      count: number;
    },
  ];
  /** Emitted when sticky routing assigns a guild to a configured node. */
  [RemediaClientEventNames.guildAssigned]: [GuildAssignment];
  /** Emitted when a guild assignment is released. */
  [RemediaClientEventNames.guildReleased]: [GuildAssignment];
  /** Emitted after a guild's destination assignment has committed. */
  [RemediaClientEventNames.guildMigrated]: [
    { guildId: string; fromNodeId: string; toNodeId: string; nodeType: NodeType },
  ];
  /** Emitted when a session is unregistered and its owned resources are released. */
  [PlaybackSessionEvents.sessionDisposed]: [
    VoiceEventIdentity & {
      /** Source generation used to correlate playback events; independent of transport replacement generations. */
      generation: number;
    },
  ];
}

/** Terminal state and source generation returned by a playback completion wait. */
export interface PlaybackCompletion {
  /** Source generation used to correlate playback events; independent of transport replacement generations. */
  generation: number;
  /** Current playback lifecycle state. */
  state: Extract<PlaybackState, 'completed' | 'failed' | 'stopped'>;
}

/** Cancellation and deadline for disposable metadata inspection. */
export interface ProbeOptions {
  /** Cancels input I/O and releases probe resources; never destroys caller-owned Readables. */
  signal?: AbortSignal;
  /** Total deadline in milliseconds, from 1 through 4294967295; defaults to 15000. */
  timeoutMs?: number;
}

/** Probe routing independent of guild affinity. */
export interface ClientProbeOptions extends ProbeOptions {
  /** Explicit configured node ID; required when multiple healthy nodes exist. */
  nodeId?: string;
}
