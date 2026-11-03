import { EmbedBuilder, type SendableChannels } from 'discord.js';
import type {
  AudioStreamInfo,
  DiscordSession,
  PlaybackState,
  RemediaClient,
} from '@remedia/client';
import { PlaybackSessionEvents, VoiceSessionEvents } from '@remedia/client';
import { displayTrack } from './command-helpers.js';
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
  filterPresets: readonly FilterPresetName[];
  volumePercent: number;
}

interface Track extends TrackRequest {
  info?: AudioStreamInfo;
}

interface TrackLifecycleMetadata {
  textChannel?: SendableChannels;
  queueGeneration: number;
  track: Track;
}

interface GuildPlayer {
  session: DiscordSession<TrackLifecycleMetadata>;
  queue: Track[];
  current: Track | undefined;
  /** The first queue's text channel, retained in memory for lifecycle announcements. */
  queueMetadata: { textChannel?: SendableChannels };
  lastError: string | undefined;
  generation: number;
  stopNotificationGeneration: number | undefined;
  operations: Promise<void>;
  filterPresets: FilterPresetName[];
  volumePercent: number;
  lifecycleUnsubscribers: Array<() => void>;
}

/** Application-owned queueing for the manual Discord harness. The native API remains single-source. */
export class DiscordPlaybackManager {
  readonly #remedia: RemediaClient;
  readonly #players = new Map<string, GuildPlayer>();
  readonly #failedNotifications = new WeakSet<Track>();
  #closed = false;

  constructor(remedia: RemediaClient) {
    this.#remedia = remedia;
  }

  async enqueue(
    guildId: string,
    channelId: string,
    botUserId: string,
    request: TrackRequest,
    textChannel?: SendableChannels,
  ): Promise<{ started: boolean; queuePosition: number; track: TrackSnapshot }> {
    this.#assertOpen();
    const player = this.#getOrCreatePlayer(guildId, channelId, botUserId, textChannel);
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
      player.stopNotificationGeneration = player.generation - 1;
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
    if (preset === null) return this.clearFilters(guildId, channelId);
    return this.setFilters(guildId, channelId, [preset]);
  }

  async setFilters(
    guildId: string,
    channelId: string,
    presets: readonly FilterPresetName[],
  ): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, async () => {
      for (const preset of presets) {
        if (!Object.hasOwn(FILTER_PRESETS, preset)) {
          throw new RangeError(`Unknown filter preset: ${preset}`);
        }
      }

      const next = [...new Set([...player.filterPresets, ...presets])];
      await this.#applyFilterPresets(player, next);
    });
  }

  async toggleFilter(
    guildId: string,
    channelId: string,
    preset: FilterPresetName,
  ): Promise<'added' | 'removed'> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      if (!Object.hasOwn(FILTER_PRESETS, preset)) {
        throw new RangeError(`Unknown filter preset: ${preset}`);
      }

      const isActive = player.filterPresets.includes(preset);
      const next = isActive
        ? player.filterPresets.filter((activePreset) => activePreset !== preset)
        : [...player.filterPresets, preset];
      await this.#applyFilterPresets(player, next);
      return isActive ? 'removed' : 'added';
    });
  }

  async clearFilters(guildId: string, channelId: string): Promise<void> {
    const player = this.#getPlayer(guildId, channelId);
    await this.#serialize(player, () => this.#clearFilterPresets(player));
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

  #getOrCreatePlayer(
    guildId: string,
    channelId: string,
    botUserId: string,
    textChannel: SendableChannels | undefined,
  ): GuildPlayer {
    const existing = this.#players.get(guildId);
    if (existing) {
      if (existing.session.channelId !== channelId) {
        throw new Error('The bot is already connected to another voice channel in this server.');
      }
      return existing;
    }

    const player: GuildPlayer = {
      session: this.#remedia.createDiscordSession<TrackLifecycleMetadata>({
        guildId,
        channelId,
        userId: botUserId,
      }),
      queue: [],
      current: undefined,
      queueMetadata: textChannel === undefined ? {} : { textChannel },
      lastError: undefined,
      generation: 0,
      stopNotificationGeneration: undefined,
      operations: Promise.resolve(),
      filterPresets: [],
      volumePercent: 100,
      lifecycleUnsubscribers: [],
    };
    this.#players.set(guildId, player);
    this.#bindLifecycleEvents(player);
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
      player.stopNotificationGeneration = undefined;
      player.current = track;
      try {
        const graph = filterGraphFor(player.filterPresets);
        if (graph === null) {
          if (player.session.filters.graph !== null) await player.session.filters.clear();
        } else if (player.session.filters.graph !== graph) {
          await player.session.filters.set(graph);
        }
        if (player.session.volume !== player.volumePercent / 100) {
          await player.session.setVolume(player.volumePercent / 100);
        }
        const metadata: TrackLifecycleMetadata = {
          ...player.queueMetadata,
          queueGeneration: generation,
          track,
        };
        track.info = await player.session.play({ url: track.url }, { customMetadata: metadata });
        if (player.generation !== generation) return;
        void this.#watchCompletion(player, generation).catch((error: unknown) => {
          console.error('Could not advance the Remedia queue:', asError(error).message);
        });
        return;
      } catch (error) {
        if (player.generation !== generation) return;
        const failure = asError(error);
        this.#notifyTrackFailure(
          player,
          {
            ...player.queueMetadata,
            queueGeneration: generation,
            track,
          },
          failure,
        );
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

  async #applyFilterPresets(player: GuildPlayer, presets: FilterPresetName[]): Promise<void> {
    if (presets.length === 0) return this.#clearFilterPresets(player);

    const graph = filterGraphFor(presets);
    if (graph !== null && player.session.filters.graph !== graph) {
      await player.session.filters.set(graph);
    }
    player.filterPresets = presets;
  }

  async #clearFilterPresets(player: GuildPlayer): Promise<void> {
    if (player.session.filters.graph !== null) await player.session.filters.clear();
    player.filterPresets = [];
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
      for (const unsubscribe of player.lifecycleUnsubscribers.splice(0)) unsubscribe();
      delete player.queueMetadata.textChannel;
      this.#players.delete(player.session.guildId);
    }
  }

  #bindLifecycleEvents(player: GuildPlayer): void {
    const onPlaybackStarted = (event: {
      generation: number;
      state: 'playing';
      info: AudioStreamInfo;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      const track = metadata.track;
      track.info ??= event.info;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Now playing',
        `${displayTrack({ url: track.url, info: event.info })}${formatTrackRequester(track)}`,
        0x57f287,
      );
    };
    const onPlaybackCompleted = (event: {
      generation: number;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Track finished',
        `${displayTrack(metadata.track)} finished playing.`,
        0x5865f2,
      );
    };
    const onPlaybackStopped = (event: {
      generation: number;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata) return;
      const expectedStop = player.stopNotificationGeneration === metadata.queueGeneration;
      if (!expectedStop && !this.#isCurrentLifecycle(player, metadata)) return;
      if (expectedStop) player.stopNotificationGeneration = undefined;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback stopped',
        `${displayTrack(metadata.track)} was stopped.`,
        0x99aab5,
      );
    };
    const onPlaybackFailed = (event: {
      generation: number;
      error: Error;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#notifyTrackFailure(player, metadata, event.error);
    };
    const onPlaybackPaused = (event: {
      generation: number;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback paused',
        `${displayTrack(metadata.track)} is paused.`,
        0xfee75c,
      );
    };
    const onPlaybackResumed = (event: {
      generation: number;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback resumed',
        `${displayTrack(metadata.track)} is playing again.`,
        0x57f287,
      );
    };
    const onPlaybackSeeked = (event: {
      generation: number;
      positionMs: number;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback position changed',
        `Seeked ${displayTrack(metadata.track)} to ${formatDuration(event.positionMs)}.`,
        0x5865f2,
      );
    };
    const onFilterChanged = (event: {
      generation: number;
      graph: string | null;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (metadata && !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Audio filter changed',
        event.graph === null ? 'The audio filter was cleared.' : 'An audio filter was applied.',
        0x5865f2,
      );
    };
    const onVolumeChanged = (event: {
      generation: number;
      volume: number;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      const metadata = event.customMetadata;
      if (metadata && !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback volume changed',
        `Master volume is now ${Math.round(event.volume * 100)}%.`,
        0x5865f2,
      );
    };
    const onVoiceStateChanged = (event: { state: string }): void => {
      const messages: Readonly<Record<string, string>> = {
        connected: 'Connected to the Discord voice channel.',
        disconnected: 'Disconnected from the Discord voice channel.',
        reconnecting: 'The Discord voice connection is reconnecting.',
      };
      const description = messages[event.state];
      if (description) {
        this.#sendLifecycleMessage(player, undefined, 'Voice connection', description, 0x5865f2);
      }
    };
    const onVoiceFailed = (event: { error: Error }): void => {
      this.#sendLifecycleMessage(
        player,
        undefined,
        'Voice connection failed',
        safeLifecycleError(event.error.message),
        0xed4245,
      );
    };

    player.session.on(PlaybackSessionEvents.playbackStarted, onPlaybackStarted);
    player.session.on(PlaybackSessionEvents.playbackCompleted, onPlaybackCompleted);
    player.session.on(PlaybackSessionEvents.playbackStopped, onPlaybackStopped);
    player.session.on(PlaybackSessionEvents.playbackFailed, onPlaybackFailed);
    player.session.on(PlaybackSessionEvents.playbackPaused, onPlaybackPaused);
    player.session.on(PlaybackSessionEvents.playbackResumed, onPlaybackResumed);
    player.session.on(PlaybackSessionEvents.playbackSeeked, onPlaybackSeeked);
    player.session.on(PlaybackSessionEvents.filterChanged, onFilterChanged);
    player.session.on(PlaybackSessionEvents.volumeChanged, onVolumeChanged);
    player.session.on(VoiceSessionEvents.voiceStateChanged, onVoiceStateChanged);
    player.session.on(VoiceSessionEvents.voiceFailed, onVoiceFailed);

    player.lifecycleUnsubscribers.push(
      () => player.session.off(PlaybackSessionEvents.playbackStarted, onPlaybackStarted),
      () => player.session.off(PlaybackSessionEvents.playbackCompleted, onPlaybackCompleted),
      () => player.session.off(PlaybackSessionEvents.playbackStopped, onPlaybackStopped),
      () => player.session.off(PlaybackSessionEvents.playbackFailed, onPlaybackFailed),
      () => player.session.off(PlaybackSessionEvents.playbackPaused, onPlaybackPaused),
      () => player.session.off(PlaybackSessionEvents.playbackResumed, onPlaybackResumed),
      () => player.session.off(PlaybackSessionEvents.playbackSeeked, onPlaybackSeeked),
      () => player.session.off(PlaybackSessionEvents.filterChanged, onFilterChanged),
      () => player.session.off(PlaybackSessionEvents.volumeChanged, onVolumeChanged),
      () => player.session.off(VoiceSessionEvents.voiceStateChanged, onVoiceStateChanged),
      () => player.session.off(VoiceSessionEvents.voiceFailed, onVoiceFailed),
    );
  }

  #isCurrentLifecycle(player: GuildPlayer, metadata: TrackLifecycleMetadata): boolean {
    return player.generation === metadata.queueGeneration && player.current === metadata.track;
  }

  #notifyTrackFailure(player: GuildPlayer, metadata: TrackLifecycleMetadata, error: Error): void {
    if (this.#failedNotifications.has(metadata.track)) return;
    this.#failedNotifications.add(metadata.track);
    this.#sendLifecycleMessage(
      player,
      metadata,
      'Playback failed',
      `Could not play ${displayTrack(metadata.track)}. ${safeLifecycleError(
        redactUrl(error.message, metadata.track.url),
      )}`,
      0xed4245,
    );
  }

  #sendLifecycleMessage(
    player: GuildPlayer,
    metadata: TrackLifecycleMetadata | undefined,
    title: string,
    description: string,
    color: number,
  ): void {
    const channel = metadata?.textChannel ?? player.queueMetadata.textChannel;
    if (!channel) return;
    const embed = new EmbedBuilder()
      .setColor(color)
      .setAuthor({ name: 'Remedia' })
      .setTitle(title)
      .setDescription(description.slice(0, 4_000))
      .setTimestamp();
    try {
      void channel.send({ embeds: [embed], allowedMentions: { parse: [] } }).catch(() => {
        if (process.env.REMEDIA_DEBUG === '1') {
          console.warn(
            `Could not send a lifecycle notification for guild ${player.session.guildId}.`,
          );
        }
      });
    } catch {
      if (process.env.REMEDIA_DEBUG === '1') {
        console.warn(
          `Could not send a lifecycle notification for guild ${player.session.guildId}.`,
        );
      }
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
    filterPresets: [...player.filterPresets],
    volumePercent: player.volumePercent,
  };
}

function filterGraphFor(presets: readonly FilterPresetName[]): string | null {
  return presets.length === 0 ? null : presets.map((preset) => FILTER_PRESETS[preset]).join(',');
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

function formatDuration(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1_000));
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return `${minutes}:${remainder.toString().padStart(2, '0')}`;
}

function formatTrackRequester(track: Track): string {
  return track.requestedBy ? `\nRequested by <@${track.requestedBy}>.` : '';
}

function safeLifecycleError(message: string): string {
  return message
    .replace(/https?:\/\/[^\s<>"']+/gi, (value) => {
      try {
        const url = new URL(value);
        return `${url.origin}${url.pathname}`;
      } catch {
        return '[media URL]';
      }
    })
    .replace(/\b(token|password|secret|authorization)(\s*[:=]\s*)[^\s,;]+/gi, '$1$2[redacted]')
    .slice(0, 800);
}
