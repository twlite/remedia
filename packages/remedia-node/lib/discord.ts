import { NativeRemedia } from '../native.js';
import { PlaybackSession } from './playback.js';
import type { VoiceConnectionState } from './types.js';

/** One independent Discord voice connection and its playback controls. */
export class DiscordSession extends PlaybackSession {
  readonly #voiceId: string;
  #disconnected = false;

  /** @internal */
  constructor(native: NativeRemedia, playbackId: string, voiceId: string, onDispose?: () => void) {
    super(native, playbackId, onDispose);
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
    try {
      await this.native.disconnectVoice(this.#voiceId);
      this.#disconnected = true;
    } finally {
      await this.releaseInput();
    }
  }

  /** Dispose a Discord session by disconnecting it; `stop()` leaves the voice transport open. */
  override async dispose(): Promise<void> {
    if (this.isDisposed) return;
    await this.disconnect();
    this.native.releaseVoiceSession(this.#voiceId);
    this.markDisposed();
  }
}
