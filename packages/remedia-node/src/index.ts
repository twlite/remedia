import { NativeRemedia } from '../native.js';
import type {
  NativeDiagnosticEvent,
  NativeMediaSource,
  NativeStreamInfo,
  NativeVoiceOptions,
} from '../native.js';

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
  | 'negotiatingDave'
  | 'connected'
  | 'reconnecting'
  | 'disconnecting'
  | 'disconnected'
  | 'failed';

export type MediaSource = { file: string } | { url: string };

export interface VoiceConnectionOptions {
  guildId: string;
  channelId: string;
  userId: string;
  sessionId: string;
  token: string;
  endpoint: string;
}

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
  submittedMs: number;
  playedMs: number | null;
}

export type DiagnosticSeverity = 'trace' | 'debug' | 'info' | 'warn' | 'error';

export type DiagnosticFieldValue =
  | { kind: 'string' | 'debug'; value: string }
  | { kind: 'integer'; value: string }
  | { kind: 'boolean'; value: boolean }
  | { kind: 'number'; value: number };

export interface DiagnosticEvent {
  timestamp: number;
  severity: DiagnosticSeverity;
  target: string;
  event: string;
  message: string | null;
  playbackId: string | null;
  voiceConnectionId: string | null;
  fields: Readonly<Record<string, DiagnosticFieldValue>>;
}

export type DebugListener = (event: DiagnosticEvent) => void;

/**
 * One embedded Remedia context. Create instances with `await Remedia.create()` so native
 * observability and optional asynchronous OpenTelemetry exporters initialize on NAPI-RS's runtime.
 */
export class Remedia {
  readonly #native: NativeRemedia;
  readonly #debugListeners = new Set<DebugListener>();
  #closed = false;

  private constructor(native: NativeRemedia) {
    this.#native = native;
    native.setDebugCallback((event) => {
      const diagnostic = toDiagnosticEvent(event);
      for (const listener of this.#debugListeners) listener(diagnostic);
    });
  }

  /** Create an independent native context using REMEDIA_* and OTEL_* environment settings. */
  static async create(): Promise<Remedia> {
    return new Remedia(await NativeRemedia.create());
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
    return new PlaybackSession(this.#native, this.#native.createPlaybackSession());
  }

  /** Connect a voice transport using credentials collected by the application's Discord Gateway. */
  async connectVoice(options: VoiceConnectionOptions): Promise<DiscordSession> {
    this.#assertOpen();
    const result = await this.#native.connectVoice(toNativeVoiceOptions(options));
    return new DiscordSession(this.#native, result.playbackId, result.voiceId);
  }

  /** Stop all playbacks, disconnect transports, flush telemetry, and release native resources. */
  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#debugListeners.clear();
    await this.#native.close();
  }

  #assertOpen(): void {
    if (this.#closed) throw new Error('Remedia instance is closed');
  }
}

/** Controls one playback session. Local sessions decode into a no-op output. */
export class PlaybackSession {
  protected readonly native: NativeRemedia;
  protected readonly playbackId: string;
  #disposed = false;

  /** @internal */
  constructor(native: NativeRemedia, playbackId: string) {
    this.native = native;
    this.playbackId = playbackId;
  }

  get state(): PlaybackState {
    return this.native.playbackState(this.playbackId) as PlaybackState;
  }

  get position(): PlaybackPosition {
    const position = this.native.playbackPosition(this.playbackId);
    return { submittedMs: position.submittedMs, playedMs: position.playedMs ?? null };
  }

  get durationMs(): number | null {
    return this.native.playbackDurationMs(this.playbackId) ?? null;
  }

  get error(): string | null {
    return this.native.playbackError(this.playbackId) ?? null;
  }

  async play(source: MediaSource): Promise<AudioStreamInfo> {
    this.#assertActive();
    const nativeInfo = await this.native.play(this.playbackId, toNativeSource(source));
    return toStreamInfo(nativeInfo);
  }

  async pause(): Promise<void> {
    this.#assertActive();
    await this.native.pause(this.playbackId);
  }

  async resume(): Promise<void> {
    this.#assertActive();
    await this.native.resume(this.playbackId);
  }

  async seek(positionMs: number): Promise<void> {
    this.#assertActive();
    await this.native.seek(this.playbackId, positionMs);
  }

  async stop(): Promise<void> {
    this.#assertActive();
    await this.native.stop(this.playbackId);
  }

  async wait(): Promise<PlaybackState> {
    this.#assertActive();
    return (await this.native.wait(this.playbackId)) as PlaybackState;
  }

  /** Stop and release this standalone playback session. */
  async dispose(): Promise<void> {
    if (this.#disposed) return;
    await this.native.disposePlaybackSession(this.playbackId);
    this.#disposed = true;
  }

  protected get isDisposed(): boolean {
    return this.#disposed;
  }

  protected markDisposed(): void {
    this.#disposed = true;
  }

  #assertActive(): void {
    if (this.#disposed) throw new Error('Playback session has been disposed');
  }
}

/** One independent Discord voice connection and its playback controls. */
export class DiscordSession extends PlaybackSession {
  readonly #voiceId: string;
  #disconnected = false;

  /** @internal */
  constructor(native: NativeRemedia, playbackId: string, voiceId: string) {
    super(native, playbackId);
    this.#voiceId = voiceId;
  }

  get connectionState(): VoiceConnectionState {
    if (this.#disconnected || this.isDisposed) return 'disconnected';
    return this.native.voiceState(this.#voiceId) as VoiceConnectionState;
  }

  get connectionError(): string | null {
    if (this.isDisposed) return null;
    return this.native.voiceError(this.#voiceId) ?? null;
  }

  /** Disconnect the transport and stop active playback. */
  async disconnect(): Promise<void> {
    if (this.#disconnected) return;
    await this.native.disconnectVoice(this.#voiceId);
    this.#disconnected = true;
  }

  /** Dispose a Discord session by disconnecting it; `stop()` leaves the voice transport open. */
  override async dispose(): Promise<void> {
    if (this.isDisposed) return;
    await this.disconnect();
    this.native.releaseVoiceSession(this.#voiceId);
    this.markDisposed();
  }
}

function toNativeSource(source: MediaSource): NativeMediaSource {
  if ('file' in source) return { kind: 'file', value: source.file };
  return { kind: 'url', value: source.url };
}

function toNativeVoiceOptions(options: VoiceConnectionOptions): NativeVoiceOptions {
  return {
    guildId: options.guildId,
    channelId: options.channelId,
    userId: options.userId,
    sessionId: options.sessionId,
    token: options.token,
    endpoint: options.endpoint,
  };
}

function toStreamInfo(info: NativeStreamInfo): AudioStreamInfo {
  return {
    streamIndex: info.streamIndex,
    codec: info.codec,
    sampleRate: info.sampleRate ?? null,
    channels: info.channels ?? null,
    channelLayout: info.channelLayout ?? null,
    durationMs: info.durationMs ?? null,
    bitRate: info.bitRate ?? null,
    metadata: info.metadata,
  };
}

function toDiagnosticEvent(event: NativeDiagnosticEvent): DiagnosticEvent {
  const severity = event.severity;
  if (!isDiagnosticSeverity(severity))
    throw new Error(`Unknown native diagnostic severity: ${severity}`);
  const fields: Record<string, DiagnosticFieldValue> = {};
  for (const [key, field] of Object.entries(event.fields)) {
    if (field.kind === 'string' || field.kind === 'debug') {
      if (field.stringValue === undefined)
        throw new Error(`Invalid native diagnostic field: ${key}`);
      fields[key] = { kind: field.kind, value: field.stringValue };
    } else if (field.kind === 'integer') {
      if (field.stringValue === undefined)
        throw new Error(`Invalid native diagnostic field: ${key}`);
      fields[key] = { kind: 'integer', value: field.stringValue };
    } else if (field.kind === 'boolean') {
      if (field.boolValue === undefined) throw new Error(`Invalid native diagnostic field: ${key}`);
      fields[key] = { kind: 'boolean', value: field.boolValue };
    } else if (field.kind === 'number') {
      if (field.numberValue === undefined)
        throw new Error(`Invalid native diagnostic field: ${key}`);
      fields[key] = { kind: 'number', value: field.numberValue };
    } else {
      throw new Error(`Unknown native diagnostic field kind: ${field.kind}`);
    }
  }
  return {
    timestamp: event.timestamp,
    severity,
    target: event.target,
    event: event.event,
    message: event.message ?? null,
    playbackId: event.playbackId ?? null,
    voiceConnectionId: event.voiceConnectionId ?? null,
    fields,
  };
}

function isDiagnosticSeverity(value: string): value is DiagnosticSeverity {
  return (
    value === 'trace' ||
    value === 'debug' ||
    value === 'info' ||
    value === 'warn' ||
    value === 'error'
  );
}
