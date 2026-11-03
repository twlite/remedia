import type { RemediaClient } from '@remedia/client';
import { DiscordPlaybackManager } from './playback-manager.js';

/** Owns the native engine and app-level playback lifecycle for this bot process. */
export class VoiceRuntime {
  static #active: VoiceRuntime | undefined;

  readonly #remedia: RemediaClient;
  readonly playback: DiscordPlaybackManager;
  #closePromise: Promise<void> | undefined;

  private constructor(remedia: RemediaClient) {
    this.#remedia = remedia;
    this.playback = new DiscordPlaybackManager(remedia);
  }

  static start(remedia: RemediaClient): VoiceRuntime {
    if (VoiceRuntime.#active) throw new Error('The voice runtime is already active.');
    const runtime = new VoiceRuntime(remedia);
    VoiceRuntime.#active = runtime;
    return runtime;
  }

  static get current(): VoiceRuntime {
    if (!VoiceRuntime.#active) throw new Error('The voice runtime is not ready yet.');
    return VoiceRuntime.#active;
  }

  receive(packet: unknown): void {
    this.#remedia.receive(packet);
  }

  close(): Promise<void> {
    if (this.#closePromise) return this.#closePromise;
    if (VoiceRuntime.#active === this) VoiceRuntime.#active = undefined;
    this.#closePromise = (async () => {
      try {
        await this.playback.close();
      } finally {
        await this.#remedia.close();
      }
    })();
    return this.#closePromise;
  }
}
