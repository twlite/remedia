import type { NativeDiagnosticEvent, NativeStreamInfo } from '../native.js';

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
  | 'negotiatingDave'
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
  /** Original media timeline associated with the latest submitted PCM block. */
  mediaMs: number | null;
  /** `timestamp` when derived from source PTS, `estimated` when inferred, or null. */
  mediaPositionAccuracy: 'timestamp' | 'estimated' | null;
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

export function toStreamInfo(info: NativeStreamInfo): AudioStreamInfo {
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

export function toDiagnosticEvent(event: NativeDiagnosticEvent): DiagnosticEvent {
  const severity = event.severity;
  if (!isDiagnosticSeverity(severity)) {
    throw new Error(`Unknown native diagnostic severity: ${severity}`);
  }

  const fields: Record<string, DiagnosticFieldValue> = {};
  for (const [key, field] of Object.entries(event.fields)) {
    if (field.kind === 'string' || field.kind === 'debug') {
      if (field.stringValue === undefined) {
        throw new Error(`Invalid native diagnostic field: ${key}`);
      }
      fields[key] = { kind: field.kind, value: field.stringValue };
    } else if (field.kind === 'integer') {
      if (field.stringValue === undefined) {
        throw new Error(`Invalid native diagnostic field: ${key}`);
      }
      fields[key] = { kind: 'integer', value: field.stringValue };
    } else if (field.kind === 'boolean') {
      if (field.boolValue === undefined) {
        throw new Error(`Invalid native diagnostic field: ${key}`);
      }
      fields[key] = { kind: 'boolean', value: field.boolValue };
    } else if (field.kind === 'number') {
      if (field.numberValue === undefined) {
        throw new Error(`Invalid native diagnostic field: ${key}`);
      }
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
