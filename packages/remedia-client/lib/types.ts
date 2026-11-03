import type { Readable } from 'node:stream';

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

export interface RemediaClientEvents {
  nodeConnected: [NodeSnapshot];
  nodeDisconnected: [{ node: NodeSnapshot; error: Error | null }];
  sessionResumed: [{ node: NodeSnapshot; guildIds: readonly string[] }];
  sessionLost: [{ node: NodeSnapshot; guildIds: readonly string[]; reason: string }];
  nodeEvent: [ServerLifecycleEvent];
  eventsLost: [{ nodeId: string; count: number }];
  guildAssigned: [GuildAssignment];
  guildReleased: [GuildAssignment];
}

export interface PlaybackCompletion {
  generation: number;
  state: Extract<PlaybackState, 'completed' | 'failed' | 'stopped'>;
}
