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
  VoiceConnectionState,
  VoiceLifecycleEventMap,
} from './types.js';

export type GuildBackendEvents = PlaybackLifecycleEventMap<unknown> & VoiceLifecycleEventMap;

export interface GuildBackend extends EventEmitter<GuildBackendEvents> {
  readonly nodeId: string;
  readonly nodeType: NodeType;
  readonly state: PlaybackState;
  readonly position: PlaybackPosition;
  readonly durationMs: number | null;
  readonly error: string | null;
  readonly volume: number;
  readonly filterGraph: string | null;
  readonly connectionState: VoiceConnectionState;
  readonly snapshot: PlaybackSnapshot;
  connect(options?: { signal?: AbortSignal; timeoutMs?: number }): Promise<void>;
  connectWithCredentials?(
    credentials: {
      sessionId: string;
      token: string;
      endpoint: string;
    },
    transportGeneration?: number,
  ): Promise<void>;
  suspendVoiceRecovery?(transportGeneration?: number): Promise<void>;
  failVoiceRecovery?(transportGeneration?: number): Promise<void>;
  validateIdentity?(userId: string): void;
  disconnect(transportGeneration?: number): Promise<void>;
  dispose(): Promise<void>;
  play(
    source: MediaSource,
    options?: PlayOptions<unknown>,
  ): Promise<{ info: AudioStreamInfo; generation: number }>;
  pause(): Promise<void>;
  resume(): Promise<void>;
  seek(positionMs: number): Promise<void>;
  stop(): Promise<void>;
  wait(): Promise<PlaybackCompletion>;
  setFilterGraph(graph: string | null): Promise<void>;
  setVolume(gain: number): Promise<void>;
  refresh(): Promise<PlaybackSnapshot>;
  markSessionLost(reason: string): void;
}

export interface NodeAdapter {
  readonly id: string;
  readonly type: NodeType;
  readonly status: NodeStatus;
  readonly snapshot: NodeSnapshot;
  start(): Promise<void>;
  close(): Promise<void>;
  receive(packet: unknown): void;
  createGuildSession(options: DiscordSessionOptions, onDispose: () => void): GuildBackend;
  releaseGuild(guildId: string): Promise<void>;
}

export type AssignmentStatus = GuildAssignment['status'];
