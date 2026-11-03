import type { Readable } from 'node:stream';

/** Event names shared by embedded and remote playback-session adapters. */
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

export const RemediaClientEventNames = {
  nodeConnected: 'nodeConnected',
  nodeDisconnected: 'nodeDisconnected',
  sessionResumed: 'sessionResumed',
  sessionLost: 'sessionLost',
  nodeEvent: 'nodeEvent',
  eventsLost: 'eventsLost',
  guildAssigned: 'guildAssigned',
  guildReleased: 'guildReleased',
} as const;

export type RemediaClientEventNames =
  (typeof RemediaClientEventNames)[keyof typeof RemediaClientEventNames];

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

export type VoiceConnectionState =
  | 'connecting'
  | 'negotiating_dave'
  | 'connected'
  | 'reconnecting'
  | 'disconnecting'
  | 'disconnected'
  | 'failed';

export interface AudioStreamInfo {
  streamIndex: number;
  codec: string;
  sampleRate: number | null;
  channels: number | null;
  channelLayout: string | null;
  durationMs: number | null;
  bitRate: number | null;
  metadata: Readonly<Record<string, string>>;
}

export interface PlaybackPosition {
  mediaMs: number | null;
  mediaPositionAccuracy: 'timestamp' | 'estimated' | null;
  submittedMs: number;
  playedMs: number | null;
}

export interface PlaybackSnapshot {
  state: PlaybackState;
  generation: number;
  processingMode: 'passthrough' | 'transcoding' | null;
  position: PlaybackPosition;
  durationMs: number | null;
  source: AudioStreamInfo | null;
  filterGraph: string | null;
  volume: number;
  lastError: { code: string; message: string } | null;
}

export type MediaSource =
  | { file: string }
  | { url: string }
  | { buffer: Buffer | Uint8Array }
  | { stream: Readable };

export type RemoteMediaSource = { file: string } | { url: string };

export type NodeType = 'embedded' | 'remote';
export type NodeStatus = 'connecting' | 'healthy' | 'disconnected' | 'session_lost' | 'closed';

export interface NodeStatistics {
  playersActive: number;
  playersMaximum: number;
  connectionsActive: number;
  connectionsMaximum: number;
  transportHealth: string;
  processingHealth: string;
  cpu: Readonly<Record<string, number | null>>;
  memory: Readonly<Record<string, number | null>>;
}

export interface NodeSnapshot {
  id: string;
  type: NodeType;
  status: NodeStatus;
  nodeId: string | null;
  version: string | null;
  protocolVersion: number | null;
  statistics: NodeStatistics | null;
  capacity: number;
  assignedGuilds: readonly string[];
  lastError: string | null;
}

export interface RemoteNodeAuth {
  username: string;
  password: string;
}

export interface EmbeddedNodeConfig {
  type: 'embedded';
  id: string;
  /** Maximum simultaneous guild sessions used by client-side selection. Defaults to 64. */
  capacity?: number;
}

export interface RemoteNodeConfig {
  type: 'remote';
  id: string;
  url: string;
  /** Explicit auth takes precedence over URL userinfo. */
  auth?: RemoteNodeAuth;
  /** Overrides the server's reported player maximum for client-side selection. */
  capacity?: number;
}

export type RemediaNodeConfig = EmbeddedNodeConfig | RemoteNodeConfig;

export interface DiscordGatewayVoicePayload {
  op: 4;
  d: {
    guild_id: string;
    channel_id: string | null;
    self_mute: false;
    self_deaf: false;
  };
}

export type DiscordGatewaySend = (
  guildId: string,
  payload: DiscordGatewayVoicePayload,
) => void | Promise<void>;

export interface DiscordGatewayPacket {
  t?: string;
  s?: number;
  d?: unknown;
  [key: string]: unknown;
}

export interface DiscordSessionOptions {
  guildId: string;
  channelId: string;
  userId: string;
  /** Pin this guild to a configured node. Without it, capacity-aware selection is used. */
  nodeId?: string;
}

export interface PlayOptions<Metadata = unknown> {
  /** Arbitrary application-owned value retained in this process and attached to one generation. */
  customMetadata?: Metadata;
  /** Cancels an awaited pre-play hook or stops this generation after startup. */
  signal?: AbortSignal;
}

export interface PlaybackEventIdentity<Metadata = unknown> {
  sessionId: string;
  guildId: string;
  nodeId: string | null;
  generation: number;
  customMetadata?: Metadata;
  reconstructed?: boolean;
}

export type PlaybackLifecycleEventMap<Metadata = unknown> = {
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

export type VoiceLifecycleEventMap = {
  [VoiceSessionEvents.voiceStateChanged]: [
    VoiceEventIdentity & { previousState: VoiceConnectionState; state: VoiceConnectionState },
  ];
  [VoiceSessionEvents.voiceFailed]: [VoiceEventIdentity & { state: 'failed'; error: Error }];
};

export type DiscordSessionEvents<Metadata = unknown> = PlaybackLifecycleEventMap<Metadata> &
  VoiceLifecycleEventMap & {
    [PlaybackSessionEvents.sessionDisposed]: [VoiceEventIdentity & { generation: number }];
  };

export interface BeforePlayContext<Metadata = unknown> {
  readonly source: MediaSource;
  readonly customMetadata: Metadata | undefined;
  readonly signal: AbortSignal;
}

export type BeforePlayResult = void | { cancel: true; reason?: string };
export type BeforePlayHook<Metadata = unknown> = (
  context: BeforePlayContext<Metadata>,
) => BeforePlayResult | Promise<BeforePlayResult>;

export interface DiscordConnectOptions {
  signal?: AbortSignal;
  timeoutMs?: number;
}

export interface RemoteNodeView {
  readonly id: string;
  readonly type: NodeType;
  readonly status: NodeStatus;
  readonly nodeId: string | null;
  readonly statistics: NodeStatistics | null;
  readonly assignedGuilds: readonly string[];
}

export type NodeSelection = (
  nodes: readonly NodeSnapshot[],
  context: { guildId: string; source?: MediaSource },
) => string | Promise<string>;

export interface RemediaClientOptions {
  nodes: readonly RemediaNodeConfig[];
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

export interface GuildAssignment {
  guildId: string;
  nodeId: string;
  nodeType: NodeType;
  status: 'assigned' | 'lost';
}

export interface ServerLifecycleEvent {
  nodeId: string;
  event: string;
  sessionId: string;
  sequence: number;
  data: Readonly<Record<string, unknown>>;
}

export interface RemediaClientEvents
  extends PlaybackLifecycleEventMap<unknown>, VoiceLifecycleEventMap {
  [RemediaClientEventNames.nodeConnected]: [NodeSnapshot];
  [RemediaClientEventNames.nodeDisconnected]: [{ node: NodeSnapshot; error: Error | null }];
  [RemediaClientEventNames.sessionResumed]: [{ node: NodeSnapshot; guildIds: readonly string[] }];
  [RemediaClientEventNames.sessionLost]: [
    { node: NodeSnapshot; guildIds: readonly string[]; reason: string },
  ];
  [RemediaClientEventNames.nodeEvent]: [ServerLifecycleEvent];
  [RemediaClientEventNames.eventsLost]: [{ nodeId: string; count: number }];
  [RemediaClientEventNames.guildAssigned]: [GuildAssignment];
  [RemediaClientEventNames.guildReleased]: [GuildAssignment];
  [PlaybackSessionEvents.sessionDisposed]: [VoiceEventIdentity & { generation: number }];
}

export interface PlaybackCompletion {
  generation: number;
  state: Extract<PlaybackState, 'completed' | 'failed' | 'stopped'>;
}
