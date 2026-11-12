export { DiscordSession } from './discord.js';
export type { DiscordConnectOptions, DiscordSessionOptions } from './discord.js';
export type {
  DiscordGatewayPacket,
  DiscordGatewaySend,
  DiscordGatewayVoicePayload,
} from './gateway.js';
export { PlaybackSession } from './playback.js';
export {
  PlaybackHooks,
  PlaybackSessionEvents,
  RemediaEvents,
  VoiceSessionEvents,
} from './lifecycle.js';
export { Remedia, type ProbeOptions, type RemediaOptions } from './remedia.js';
export { getFfmpegRuntime } from './native-binding.js';
export { FfmpegLoadError } from './ffmpeg-runtime.js';
export type {
  FfmpegCandidateReport,
  FfmpegFailureReason,
  FfmpegLibraryInfo,
  FfmpegRuntimeInfo,
  FfmpegSource,
} from './ffmpeg-runtime.js';
export type { MediaSource } from './input.js';
export type {
  AudioStreamInfo,
  DebugListener,
  DiagnosticEvent,
  DiagnosticFieldValue,
  DiagnosticSeverity,
  PlaybackPosition,
  PlaybackState,
  VoiceConnectionState,
} from './types.js';
export type {
  CrossfadeCurve,
  CrossfadeOptions,
  CrossfadeContinuation,
  CrossfadeTransition,
  CrossfadeStageEvent,
  StageNextOptions,
  StageSnapshot,
  BeforePlayContext,
  BeforePlayHook,
  BeforePlayResult,
  LifecycleVoiceState,
  PlayOptions,
  PlaybackEventIdentity,
  PlaybackLifecycleEvents,
  PlaybackSessionEventMap,
  RemediaLifecycleEvents,
  SessionLifecycleEvents,
  VoiceEventIdentity,
  VoiceLifecycleEvents,
} from './lifecycle.js';

export type { ResourceMemory, ResourceCpu, ProcessResourceStatistics } from './resources.js';
