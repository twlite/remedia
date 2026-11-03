import type {
  AudioStreamInfo,
  DiscordSession,
  PlaybackState,
  RemediaClient,
} from '@remedia/client';
import { FILTER_PRESETS, type FilterPresetName } from './filter-presets.js';

const MAX_QUEUED_TRACKS = 100;

export interface TrackRequest {
  url: string;
  requestedBy: string;
}

export interface TrackSnapshot extends TrackRequest {
  info: AudioStreamInfo | undefined;
}

export interface PlaybackSnapshot {
  channelId: string;
  connectionState: string;
  state: PlaybackState;
  positionMs: number;
  positionEstimated: boolean;
  durationMs: number | null;
  current: TrackSnapshot | null;
  queue: readonly TrackRequest[];
  lastError: string | null;
  filterPreset: FilterPresetName | null;
  volumePercent: number;
}

interface Track extends TrackRequest {
  info?: AudioStreamInfo;
}

interface GuildPlayer {
  session: DiscordSession;
  queue: Track[];
  current: Track | undefined;
  lastError: string | undefined;
  generation: number;
  operations: Promise<void>;
  filterPreset: FilterPresetName | null;
  volumePercent: number;
}

/** Application-owned queueing for the manual Discord harness. The native API remains single-source. */
export class DiscordPlaybackManager {
  readonly #remedia: RemediaClient;
  readonly #players = new Map<string, GuildPlayer>();
  #closed = false;

  constructor(remedia: RemediaClient) {
    this.#remedia = remedia;
  }

  async enqueue(
    guildId: string,
    channelId: string,
    botUserId: string,
    request: TrackRequest,
  ): Promise<{ started: boolean; queuePosition: number; track: TrackSnapshot }> {
    this.#assertOpen();
    const player = this.#getOrCreatePlayer(guildId, channelId, botUserId);
    return this.#serialize(player, async () => {
      if (player.session.connectionState !== 'connected') {
        try {
          await player.session.connect();
        } catch (error) {
          await this.#removePlayer(player);
          throw error;
        }
      }
      if (process.env.REMEDIA_DEBUG === '1' && player.session.nodeId) {
        console.info(`Guild ${guildId} assigned to Remedia node ${player.session.nodeId}.`);
      }

      if (player.queue.length >= MAX_QUEUED_TRACKS) {
        throw new Error(`The queue is full (maximum ${MAX_QUEUED_TRACKS} upcoming tracks).`);
      }
      const track: Track = { ...request };
      player.lastError = undefined;
      const shouldStart = player.current === undefined && player.queue.length === 0;
      player.queue.push(track);
      if (shouldStart) await this.#startNext(player);

      const index = player.current === track ? 0 : player.queue.indexOf(track) + 1;
      return {
        started: player.current === track,
        queuePosition: index,
        track: toTrackSnapshot(track),
      };
    });
  }

  async pause(guildId: string, channelId: string): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, async () => {
      if (player.session.state === 'paused') return;
      if (player.session.state !== 'playing')
        throw new Error('There is no playing track to pause.');
      await player.session.pause();
    });
  }

  async resume(guildId: string, channelId: string): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, async () => {
      if (player.session.state === 'playing') return;
      if (player.session.state !== 'paused') throw new Error('There is no paused track to resume.');
      await player.session.resume();
    });
  }

  async seek(guildId: string, channelId: string, positionMs: number): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, async () => {
      if (!player.current) throw new Error('There is no current track to seek.');
      await player.session.seek(positionMs);
    });
  }

  async stop(guildId: string, channelId: string): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, async () => {
      player.generation += 1;
      player.current = undefined;
      player.queue.length = 0;
      if (isActivePlayback(player.session.state)) await player.session.stop();
      player.lastError = undefined;
    });
  }

  async skip(guildId: string, channelId: string): Promise<TrackSnapshot | null> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      if (!player.current) throw new Error('There is no current track to skip.');
      player.generation += 1;
      player.current = undefined;
      if (isActivePlayback(player.session.state)) await player.session.stop();
      await this.#startNext(player);
      return player.current ? toTrackSnapshot(player.current) : null;
    });
  }

  async clearQueue(guildId: string, channelId: string): Promise<number> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, () => {
      const removed = player.queue.length;
      player.queue.length = 0;
      return Promise.resolve(removed);
    });
  }

  async setFilter(
    guildId: string,
    channelId: string,
    preset: FilterPresetName | null,
  ): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, async () => {
      if (preset !== null && !Object.hasOwn(FILTER_PRESETS, preset)) {
        throw new RangeError(`Unknown filter preset: ${preset}`);
      }
      if (preset === null) await player.session.filters.clear();
      else await player.session.filters.set(FILTER_PRESETS[preset]);
      player.filterPreset = preset;
    });
  }

  async setVolume(guildId: string, channelId: string, percent: number): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    if (!Number.isInteger(percent) || percent < 0 || percent > 200) {
      throw new RangeError('Volume must be an integer from 0 to 200 percent.');
    }
    await this.#serialize(player, async () => {
      await player.session.setVolume(percent / 100);
      player.volumePercent = percent;
    });
  }

  async snapshot(guildId: string, channelId: string): Promise<PlaybackSnapshot> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, () => Promise.resolve(toPlaybackSnapshot(player)));
  }

  async disconnect(guildId: string, channelId: string): Promise<boolean> {
    const player = this.#players.get(guildId);
    if (!player) return false;
    if (player.session.channelId !== channelId) {
      throw new Error('Join the same voice channel as the bot to control playback.');
    }
    await this.#serialize(player, async () => this.#removePlayer(player));
    return true;
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    const results = await Promise.allSettled(
      [...this.#players.values()].map((player) =>
        this.#serialize(player, async () => this.#removePlayer(player)),
      ),
    );
    const failures = results.flatMap((result) =>
      result.status === 'rejected' ? [result.reason] : [],
    );
    if (failures.length > 0) {
      throw new AggregateError(failures, 'one or more Discord playback sessions failed to close');
    }
  }

  #getOrCreatePlayer(guildId: string, channelId: string, botUserId: string): GuildPlayer {
    const existing = this.#players.get(guildId);
    if (existing) {
      if (existing.session.channelId !== channelId) {
        throw new Error('The bot is already connected to another voice channel in this server.');
      }
      return existing;
    }

    const player: GuildPlayer = {
      session: this.#remedia.createDiscordSession({ guildId, channelId, userId: botUserId }),
      queue: [],
      current: undefined,
      lastError: undefined,
      generation: 0,
      operations: Promise.resolve(),
      filterPreset: null,
      volumePercent: 100,
    };
    this.#players.set(guildId, player);
    return player;
  }

  #getPlayer(guildId: string, channelId: string): GuildPlayer {
    const player = this.#players.get(guildId);
    if (!player) throw new Error('There is no Remedia voice session in this server.');
    if (player.session.channelId !== channelId) {
      throw new Error('Join the same voice channel as the bot to control playback.');
    }
    return player;
  }

  async #startNext(player: GuildPlayer): Promise<void> {
    let firstFailure: Error | undefined;
    while (!player.current && player.queue.length > 0) {
      const track = player.queue.shift();
      if (!track) return;
      const generation = ++player.generation;
      player.current = track;
      try {
        const graph = player.filterPreset === null ? null : FILTER_PRESETS[player.filterPreset];
        if (graph === null) {
          if (player.session.filters.graph !== null) await player.session.filters.clear();
        } else if (player.session.filters.graph !== graph) {
          await player.session.filters.set(graph);
        }
        if (player.session.volume !== player.volumePercent / 100) {
          await player.session.setVolume(player.volumePercent / 100);
        }
        track.info = await player.session.play({ url: track.url });
        if (player.generation !== generation) return;
        void this.#watchCompletion(player, generation).catch((error: unknown) => {
          console.error('Could not advance the Remedia queue:', asError(error).message);
        });
        return;
      } catch (error) {
        if (player.generation !== generation) return;
        const failure = asError(error);
        firstFailure ??= failure;
        player.lastError = `A track failed to open or decode: ${redactUrl(failure.message, track.url)}`;
        player.current = undefined;
        console.error(`Remedia skipped a track that failed in guild ${player.session.guildId}.`);
        if (isActivePlayback(player.session.state)) {
          await player.session.stop().catch(() => {});
        }
      }
    }
    if (firstFailure) throw firstFailure;
  }

  async #watchCompletion(player: GuildPlayer, generation: number): Promise<void> {
    let state: PlaybackState;
    try {
      state = await player.session.wait();
    } catch (error) {
      if (
        asError(error).name === 'SessionLostError' ||
        asError(error).name === 'NodeUnavailableError'
      ) {
        player.lastError = 'The assigned Remedia node is unavailable; this guild was not migrated.';
        return;
      }
      state = 'failed';
      player.lastError = asError(error).message;
    }

    if (state !== 'completed' && state !== 'failed') return;
    await this.#serialize(player, async () => {
      if (player.generation !== generation || !player.current) return;
      if (state === 'failed') {
        const message = player.session.error ?? 'Playback failed.';
        player.lastError = `Playback failed: ${redactUrl(message, player.current.url)}`;
      }
      player.current = undefined;
      await this.#startNext(player);
    });
  }

  async #removePlayer(player: GuildPlayer): Promise<void> {
    player.generation += 1;
    player.current = undefined;
    player.queue.length = 0;
    try {
      await player.session.dispose();
    } finally {
      this.#players.delete(player.session.guildId);
    }
  }

  #serialize<T>(player: GuildPlayer, operation: () => Promise<T>): Promise<T> {
    const result = player.operations.then(operation, operation);
    player.operations = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }

  #assertOpen(): void {
    if (this.#closed) throw new Error('Discord playback manager is closed.');
  }
}

function toTrackSnapshot(track: Track): TrackSnapshot {
  return { ...track, info: track.info };
}

function toPlaybackSnapshot(player: GuildPlayer): PlaybackSnapshot {
  const position = player.session.position;
  return {
    channelId: player.session.channelId,
    connectionState: player.session.connectionState,
    state: player.session.state,
    positionMs: position.mediaMs ?? position.submittedMs,
    positionEstimated: position.playedMs === null || position.mediaPositionAccuracy === 'estimated',
    durationMs: player.session.durationMs,
    current: player.current ? toTrackSnapshot(player.current) : null,
    queue: player.queue.map(({ url, requestedBy }) => ({ url, requestedBy })),
    lastError: player.lastError ?? null,
    filterPreset: player.filterPreset,
    volumePercent: player.volumePercent,
  };
}

function isActivePlayback(state: PlaybackState): boolean {
  return (
    state === 'starting' ||
    state === 'playing' ||
    state === 'pausing' ||
    state === 'resuming' ||
    state === 'paused' ||
    state === 'stopping'
  );
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function redactUrl(message: string, source: string): string {
  let display = source;
  try {
    const url = new URL(source);
    display = `${url.origin}${url.pathname}`;
  } catch {
    // The command validates URLs before they reach the manager.
  }
  return message.replaceAll(source, display).slice(0, 300);
}
