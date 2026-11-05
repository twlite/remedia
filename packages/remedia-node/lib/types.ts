import type { NativeDiagnosticEvent, NativeStreamInfo } from '../native.js';

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
  | 'negotiatingDave'
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
  /** Original media timeline associated with the latest submitted PCM block. */
  mediaMs: number | null;
  /** `timestamp` when derived from source PTS, `estimated` when inferred, or null. */
  mediaPositionAccuracy: 'timestamp' | 'estimated' | null;
  /**
   * Duration of audio submitted to the output in milliseconds; may differ from source position after filtering.
   */
  submittedMs: number;
  /** Output-reported played duration in milliseconds, or null when the output does not provide it. */
  playedMs: number | null;
}

/** Severity of a structured native tracing event. */
export type DiagnosticSeverity = 'trace' | 'debug' | 'info' | 'warn' | 'error';

/** Typed diagnostic value; integers remain decimal strings to preserve precision in JavaScript. */
export type DiagnosticFieldValue =
  | {
      /** Discriminant describing the representation of the diagnostic field. */
      kind: 'string' | 'debug';
      /** Diagnostic field value in the representation selected by kind. */
      value: string;
    }
  | {
      /** Discriminant describing the representation of the diagnostic field. */
      kind: 'integer';
      /** Diagnostic field value in the representation selected by kind. */
      value: string;
    }
  | {
      /** Discriminant describing the representation of the diagnostic field. */
      kind: 'boolean';
      /** Diagnostic field value in the representation selected by kind. */
      value: boolean;
    }
  | {
      /** Discriminant describing the representation of the diagnostic field. */
      kind: 'number';
      /** Diagnostic field value in the representation selected by kind. */
      value: number;
    };

/** Structured native diagnostic event forwarded through the bounded diagnostic queues. */
export interface DiagnosticEvent {
  /** Unix timestamp of the diagnostic event in milliseconds. */
  timestamp: number;
  /** Native tracing severity associated with the event. */
  severity: DiagnosticSeverity;
  /** Native tracing target or module that emitted the diagnostic. */
  target: string;
  /** Server or diagnostic event name. */
  event: string;
  /** Human-readable diagnostic or failure description. */
  message: string | null;
  /** Native playback identity, or null for diagnostics without playback context. */
  playbackId: string | null;
  /** Native voice connection identity, or null for diagnostics without voice context. */
  voiceConnectionId: string | null;
  /** Typed structured diagnostic fields; integers are represented as decimal strings. */
  fields: Readonly<Record<string, DiagnosticFieldValue>>;
}

/**
 * Callback receiving a structured native diagnostic event.
 * @param event - Structured native diagnostic delivered by the bounded event queue.
 */
export type DebugListener = (event: DiagnosticEvent) => void;

/**
 * Normalize native optional stream metadata into explicit nullable public fields.
 * @param info - Native stream metadata to normalize into the public nullable representation.
 * @returns Stream metadata using the public AudioStreamInfo representation.
 */
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

/**
 * Normalize a structured native diagnostic; invalid severity or field representations throw.
 * @param event - Event name identifying the typed notification or Gateway voice dispatch.
 * @returns A diagnostic event with typed field values.
 */
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
