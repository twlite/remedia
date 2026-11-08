import { randomUUID } from 'node:crypto';
import { EmbedBuilder, type SendableChannels } from 'discord.js';
import type {
  AudioStreamInfo,
  DiscordSessionOptions,
  PlaybackLifecycleEventMap,
  DiscordSession,
  PlaybackState,
  RemediaClient,
} from '@remedia/client';
import { PlaybackSessionEvents, VoiceSessionEvents } from '@remedia/client';
import { displayTrack, stripControlCharacters } from './command-helpers.js';
import { FILTER_PRESETS, type FilterPresetName } from './filter-presets.js';

const MAX_QUEUED_TRACKS = 100;

export const LOOP_MODES = ['off', 'track', 'queue'] as const;
export type LoopMode = (typeof LOOP_MODES)[number];

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
  loopMode: LoopMode;
}

export interface PlaybackStateChange {
  previousState: PlaybackState;
  changed: boolean;
  current: TrackSnapshot | null;
}

export interface SeekActionContext {
  current: TrackSnapshot;
  previousMediaPositionMs: number | null;
  previousPositionEstimated: boolean;
}

export interface SkipActionContext {
  skipped: TrackSnapshot;
  next: TrackSnapshot | null;
  queuedNext: TrackSnapshot | null;
}

interface Track extends TrackRequest {
  info?: AudioStreamInfo;
}

interface TrackLifecycleMetadata {
  textChannel?: SendableChannels;
  queueGeneration: number;
  track: Track;
}

type PlaybackEvents = PlaybackLifecycleEventMap<TrackLifecycleMetadata>;

interface CrossfadeTest {
  first: Track;
  second: Track;
  entryId: string;
  firstGeneration: number | undefined;
  secondGeneration: number | undefined;
  attempted: Set<number>;
  notifications: Set<string>;
  revision: number;
  stage: { generation: number; sourceRevision: number } | undefined;
  allowPreparation: boolean;
  firstEnded: boolean;
}

interface GuildPlayer {
  session: DiscordSession<TrackLifecycleMetadata>;
  queue: Track[];
  current: Track | undefined;
  /** The first queue's text channel, retained in memory for lifecycle announcements. */
  queueMetadata: { textChannel?: SendableChannels };
  lastError: string | undefined;
  generation: number;
  operations: Promise<void>;
  filterPresets: FilterPresetName[];
  volumePercent: number;
  loopMode: LoopMode;
  lifecycleUnsubscribers: Array<() => void>;
  crossfadeSession: boolean;
  crossfadeTest: CrossfadeTest | undefined;
}

/** Application-owned queueing and explicit crossfade smoke tests for the Discord harness. */
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
    if (player.crossfadeSession) {
      throw new Error('This session is reserved for /crossfade. Use /disconnect before /play.');
    }
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

  /**
   * Start a two-source smoke test in a new, explicitly opted-in session.
   * @param guildId - Invoking guild identity.
   * @param channelId - Invoking user's voice channel.
   * @param botUserId - Bot user identity used by the Gateway adapter.
   * @param first - First direct media URL and requester.
   * @param second - Upcoming direct media URL and requester.
   * @param textChannel - Destination for generation-correlated test diagnostics.
   * @returns Selected node identifier after connecting and starting the first source.
   * @throws If a guild session already exists or no healthy capable node is available.
   */
  async crossfade(
    guildId: string,
    channelId: string,
    botUserId: string,
    first: TrackRequest,
    second: TrackRequest,
    textChannel: SendableChannels,
  ): Promise<string> {
    this.#assertOpen();
    if (this.#players.has(guildId)) {
      throw new Error(
        '/crossfade requires a new voice session. Use /disconnect first; existing playback was left running.',
      );
    }
    const node = this.#remedia.nodes.find(
      (candidate) =>
        candidate.status === 'healthy' &&
        candidate.capabilities?.crossfade.available &&
        candidate.assignedGuilds.length < candidate.capacity,
    );
    if (!node) {
      throw new Error(
        'No healthy Remedia node advertises available crossfade capacity. For remote mode, enable audio.crossfade.enabled in remedia.config.json and reconnect the bot; older servers may not support crossfade.',
      );
    }
    const player = this.#getOrCreatePlayer(guildId, channelId, botUserId, textChannel, {
      nodeId: node.id,
      crossfade: { enabled: true, durationMs: 5000, prepareBeforeMs: 30000, curve: 'equalPower' },
    });
    const firstTrack: Track = { ...first };
    const secondTrack: Track = { ...second };
    player.crossfadeTest = {
      first: firstTrack,
      second: secondTrack,
      entryId: randomUUID(),
      firstGeneration: undefined,
      secondGeneration: undefined,
      attempted: new Set(),
      notifications: new Set(),
      revision: 0,
      stage: undefined,
      allowPreparation: true,
      firstEnded: false,
    };
    player.queue.push(firstTrack, secondTrack);
    return this.#serialize(player, async () => {
      try {
        await player.session.connect();
        this.#crossfadeMessage(
          player,
          'Crossfade session enabled',
          `Node: ${node.id} (${node.type}). Fade: 5000 ms; preparation lead: 30000 ms; equalPower. Entry: ${player.crossfadeTest!.entryId}.`,
        );
        await this.#startNext(player);
        return node.id;
      } catch (error) {
        const failure = crossfadeError(error);
        try {
          await this.#removePlayer(player);
        } catch (cleanup) {
          throw new Error(`${failure}. Session cleanup also failed: ${crossfadeError(cleanup)}`, {
            cause: cleanup,
          });
        }
        throw new Error(failure, { cause: error });
      }
    });
  }

  async pause(guildId: string, channelId: string): Promise<void> {
    await this.pauseWithContext(guildId, channelId);
  }

  async pauseWithContext(guildId: string, channelId: string): Promise<PlaybackStateChange> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      const previousState = player.session.state;
      const current = player.current ? toTrackSnapshot(player.current) : null;
      if (previousState === 'paused') return { previousState, changed: false, current };
      if (previousState !== 'playing') throw new Error('There is no playing track to pause.');
      await player.session.pause();
      return { previousState, changed: true, current };
    });
  }

  async resume(guildId: string, channelId: string): Promise<void> {
    await this.resumeWithContext(guildId, channelId);
  }

  async resumeWithContext(guildId: string, channelId: string): Promise<PlaybackStateChange> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      const previousState = player.session.state;
      const current = player.current ? toTrackSnapshot(player.current) : null;
      if (previousState === 'playing') return { previousState, changed: false, current };
      if (previousState !== 'paused') throw new Error('There is no paused track to resume.');
      await player.session.resume();
      return { previousState, changed: true, current };
    });
  }

  async seek(guildId: string, channelId: string, positionMs: number): Promise<void> {
    await this.seekWithContext(guildId, channelId, positionMs);
  }

  async seekWithContext(
    guildId: string,
    channelId: string,
    positionMs: number,
  ): Promise<SeekActionContext> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      if (!player.current) throw new Error('There is no current track to seek.');
      const position = player.session.position;
      const context = {
        current: toTrackSnapshot(player.current),
        previousMediaPositionMs: position.mediaMs,
        previousPositionEstimated:
          position.playedMs === null || position.mediaPositionAccuracy === 'estimated',
      };
      await player.session.seek(positionMs);
      // Seeking during overlap can restore the outgoing source. Report the
      // source the engine retained rather than the abandoned incoming source.
      if (player.current) context.current = toTrackSnapshot(player.current);
      return context;
    });
  }

  async stop(guildId: string, channelId: string): Promise<void> {
    await this.stopWithContext(guildId, channelId);
  }

  async stopWithContext(
    guildId: string,
    channelId: string,
  ): Promise<{ current: TrackSnapshot | null }> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      const current = player.current ? toTrackSnapshot(player.current) : null;
      player.crossfadeTest = undefined;
      player.generation += 1;
      player.current = undefined;
      player.queue.length = 0;
      if (isActivePlayback(player.session.state)) await player.session.stop();
      player.lastError = undefined;
      return { current };
    });
  }

  async skip(guildId: string, channelId: string): Promise<TrackSnapshot | null> {
    return (await this.skipWithContext(guildId, channelId)).next;
  }

  async skipWithContext(guildId: string, channelId: string): Promise<SkipActionContext> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      if (!player.current) throw new Error('There is no current track to skip.');
      player.crossfadeTest = undefined;
      player.generation += 1;
      const skipped = player.current;
      player.current = undefined;
      if (isActivePlayback(player.session.state)) await player.session.stop();
      if (player.loopMode === 'queue') player.queue.push(skipped);
      await this.#startNext(player);
      return {
        skipped: toTrackSnapshot(skipped),
        next: player.current ? toTrackSnapshot(player.current) : null,
        queuedNext: !player.current && player.queue[0] ? toTrackSnapshot(player.queue[0]) : null,
      };
    });
  }

  async clearQueue(guildId: string, channelId: string): Promise<number> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, () => {
      const removed = player.queue.length;
      player.queue.length = 0;
      if (player.crossfadeTest) {
        player.crossfadeTest.allowPreparation = false;
        return player.session.clearStage().then(() => removed);
      }
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
    return (await this.toggleFilterWithContext(guildId, channelId, preset)).change;
  }

  async toggleFilterWithContext(
    guildId: string,
    channelId: string,
    preset: FilterPresetName,
  ): Promise<{ change: 'added' | 'removed'; activePresets: readonly FilterPresetName[] }> {
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
      return {
        change: isActive ? 'removed' : 'added',
        activePresets: [...player.filterPresets],
      };
    });
  }

  async clearFilters(guildId: string, channelId: string): Promise<void> {
    await this.clearFiltersWithContext(guildId, channelId);
  }

  async clearFiltersWithContext(
    guildId: string,
    channelId: string,
  ): Promise<readonly FilterPresetName[]> {
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      const previous = [...player.filterPresets];
      await this.#clearFilterPresets(player);
      return previous;
    });
  }

  async setVolume(guildId: string, channelId: string, percent: number): Promise<void> {
    await this.setVolumeWithContext(guildId, channelId, percent);
  }

  async setVolumeWithContext(
    guildId: string,
    channelId: string,
    percent: number,
  ): Promise<{ previousPercent: number }> {
    const player = this.#getPlayer(guildId, channelId);
    if (!Number.isInteger(percent) || percent < 0 || percent > 200) {
      throw new RangeError('Volume must be an integer from 0 to 200 percent.');
    }
    return this.#serialize(player, async () => {
      const previousPercent = player.volumePercent;
      await player.session.setVolume(percent / 100);
      player.volumePercent = percent;
      return { previousPercent };
    });
  }

  async setLoopMode(guildId: string, channelId: string, mode: LoopMode): Promise<void> {
    await this.setLoopModeWithContext(guildId, channelId, mode);
  }

  async setLoopModeWithContext(
    guildId: string,
    channelId: string,
    mode: LoopMode,
  ): Promise<{ previousMode: LoopMode }> {
    if (this.#players.get(guildId)?.crossfadeTest)
      throw new Error('Looping is disabled during the two-track crossfade test.');
    if (!LOOP_MODES.includes(mode)) throw new RangeError(`Unknown loop mode: ${mode}`);
    const player = this.#getPlayer(guildId, channelId);
    return this.#serialize(player, async () => {
      const previousMode = player.loopMode;
      player.loopMode = mode;
      return { previousMode };
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
    options: Pick<DiscordSessionOptions, 'nodeId' | 'crossfade'> = {},
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
        ...options,
      }),
      queue: [],
      current: undefined,
      queueMetadata: textChannel === undefined ? {} : { textChannel },
      lastError: undefined,
      generation: 0,
      operations: Promise.resolve(),
      filterPresets: [],
      volumePercent: 100,
      loopMode: 'off',
      lifecycleUnsubscribers: [],
      crossfadeSession: Boolean(options.crossfade),
      crossfadeTest: undefined,
    };
    this.#players.set(guildId, player);
    this.#bindLifecycleEvents(player);
    if (player.crossfadeSession) this.#bindCrossfadeEvents(player);
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
    if (player.session.connectionState !== 'connected') return;
    let firstFailure: Error | undefined;
    while (!player.current && player.queue.length > 0) {
      const track = player.queue.shift();
      if (!track) return;
      const generation = ++player.generation;
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
        if (!player.crossfadeTest)
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
      if (state === 'completed') {
        if (player.loopMode === 'track') player.queue.unshift(player.current);
        else if (player.loopMode === 'queue') player.queue.push(player.current);
      }
      player.current = undefined;
      await this.#startNext(player);
    });
  }

  async #removePlayer(player: GuildPlayer): Promise<void> {
    player.crossfadeTest = undefined;
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
    const onPlaybackStarted = (event: PlaybackEvents['playbackStarted'][0]): void => {
      if (this.#crossfadeStarted(player, event)) return;
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      const track = metadata.track;
      track.info ??= event.info;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Now playing',
        displayTrack({ url: track.url, info: event.info }),
        0x57f287,
      );
    };
    const onPlaybackCompleted = (event: PlaybackEvents['playbackCompleted'][0]): void => {
      if (this.#crossfadeEnded(player, event)) return;
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Track ended',
        `Finished ${displayTrack(metadata.track)}.`,
        0x5865f2,
      );
    };
    const onPlaybackStopped = (event: { customMetadata?: TrackLifecycleMetadata }): void => {
      const metadata = event.customMetadata;
      if (!metadata || this.#closed || this.#players.get(player.session.guildId) !== player) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback stopped',
        `Stopped ${displayTrack(metadata.track)}.`,
        0x5865f2,
      );
    };
    const onPlaybackFailed = (event: {
      generation: number;
      error: Error;
      customMetadata?: TrackLifecycleMetadata;
    }): void => {
      if (this.#crossfadeEnded(player, event)) return;
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#notifyTrackFailure(player, metadata, event.error);
    };
    const onPlaybackPaused = (event: { customMetadata?: TrackLifecycleMetadata }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback paused',
        `Paused ${displayTrack(metadata.track)}.`,
        0x5865f2,
      );
    };
    const onPlaybackResumed = (event: { customMetadata?: TrackLifecycleMetadata }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback resumed',
        `Resumed ${displayTrack(metadata.track)}.`,
        0x57f287,
      );
    };
    const onPlaybackSeeked = (event: {
      customMetadata?: TrackLifecycleMetadata;
      positionMs: number;
    }): void => {
      const metadata = event.customMetadata;
      if (!metadata || !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Playback seeked',
        `Seeked to ${formatLifecyclePosition(event.positionMs)} in ${displayTrack(metadata.track)}.`,
        0x5865f2,
      );
    };
    const onFilterChanged = (event: { customMetadata?: TrackLifecycleMetadata }): void => {
      const metadata = event.customMetadata;
      if (metadata && !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Audio filters changed',
        metadata
          ? `Updated filters for ${displayTrack(metadata.track)}.`
          : 'Updated audio filters.',
        0x5865f2,
      );
    };
    const onVolumeChanged = (event: {
      customMetadata?: TrackLifecycleMetadata;
      volume: number;
    }): void => {
      const metadata = event.customMetadata;
      if (metadata && !this.#isCurrentLifecycle(player, metadata)) return;
      this.#sendLifecycleMessage(
        player,
        metadata,
        'Volume changed',
        `Volume is now ${Math.round(event.volume * 100)}%.`,
        0x5865f2,
      );
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
    const onVoiceStateChanged = (event: { state: string }): void => {
      if (event.state !== 'connected') return;
      void this.#serialize(player, async () => {
        if (this.#closed || player.current || player.queue.length === 0) return;
        await this.#startNext(player);
      }).catch((error: unknown) => {
        console.error(
          'Could not resume the Remedia queue after voice recovery:',
          asError(error).message,
        );
      });
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

  #crossfadeMessage(player: GuildPlayer, title: string, description: string, key?: string): void {
    const test = player.crossfadeTest;
    if (!test || this.#closed || this.#players.get(player.session.guildId) !== player) return;
    if (key && test.notifications.has(key)) return;
    if (key) test.notifications.add(key);
    this.#sendLifecycleMessage(player, undefined, title, description, 0x5865f2);
  }

  #crossfadeStarted(player: GuildPlayer, event: PlaybackEvents['playbackStarted'][0]): boolean {
    const test = player.crossfadeTest;
    if (!test) return false;
    const track = event.customMetadata?.track;
    if (track === test.first) {
      if (test.firstGeneration !== undefined) return true;
      test.firstGeneration = event.generation;
      track.info = event.info;
      this.#crossfadeMessage(
        player,
        'Now playing',
        `First source started. Generation ${event.generation}; duration ${event.info.durationMs ?? 'unknown'} ms.`,
        `start:${event.generation}`,
      );
      if (event.info.durationMs === null)
        this.#crossfadeMessage(
          player,
          'Preparation unavailable',
          'The first source has unknown duration. No end-based preparation can be scheduled; normal completion will play the second URL.',
        );
      return true;
    }
    if (track !== test.second) return false;
    const transition = event.transition;
    if (
      transition &&
      (transition.entryId !== test.entryId ||
        transition.outgoingGeneration !== test.firstGeneration ||
        event.generation <= transition.outgoingGeneration)
    )
      return true;
    if (!transition && !test.firstEnded) return true;
    if (test.secondGeneration === event.generation) return true;
    test.secondGeneration = event.generation;
    track.info = event.info;
    player.current = track;
    player.queue = player.queue.filter((queued) => queued !== track);
    this.#crossfadeMessage(
      player,
      transition ? 'Transition started' : 'Now playing',
      transition
        ? `Second source is audible. Generation ${event.generation}, outgoing ${transition.outgoingGeneration}; entry ${test.entryId}; overlap 5000 ms. No second play() call was issued.`
        : `Second source started normally after completion without promotion. Generation ${event.generation}; entry ${test.entryId}.`,
      `start:${event.generation}`,
    );
    return true;
  }

  #crossfadeEnded(
    player: GuildPlayer,
    event: {
      generation: number;
      customMetadata?: TrackLifecycleMetadata;
      continuation?: PlaybackEvents['playbackCompleted'][0]['continuation'];
      error?: Error;
    },
  ): boolean {
    const test = player.crossfadeTest;
    if (!test) return false;
    const track = event.customMetadata?.track;
    if (track !== test.first && track !== test.second) return false;
    if (test.notifications.has(`end:${event.generation}`)) return true;
    if (
      track === test.first &&
      test.firstGeneration !== undefined &&
      event.generation !== test.firstGeneration
    )
      return true;
    if (track === test.second && event.generation !== test.secondGeneration) return true;
    test.notifications.add(`end:${event.generation}`);
    const reason = event.error ? ` Failure: ${crossfadeError(event.error)}.` : '';
    if (event.error) {
      this.#failedNotifications.add(track);
      player.lastError = crossfadeError(event.error);
    }
    if (track === test.first) {
      test.firstEnded = true;
      test.allowPreparation = false;
      const continuation = event.continuation;
      if (continuation) {
        if (
          continuation.entryId !== test.entryId ||
          continuation.generation <= event.generation ||
          (test.secondGeneration !== undefined && continuation.generation !== test.secondGeneration)
        ) {
          player.lastError =
            'Unexpected crossfade continuation identity; no replacement play() was issued.';
          this.#crossfadeMessage(player, 'Crossfade reconciliation failed', player.lastError);
          return true;
        }
        test.secondGeneration = continuation.generation;
        player.current = test.second;
        player.queue = player.queue.filter((queued) => queued !== test.second);
        this.#crossfadeMessage(
          player,
          'Second source promoted',
          `First source completed: generation ${event.generation}. Continuation: type=promoted, generation=${continuation.generation}, entryId=${continuation.entryId}. Remedia owns playback; no second play() call was issued.`,
          `promotion:${continuation.generation}`,
        );
        return true;
      }
      this.#crossfadeMessage(
        player,
        'Track ended',
        `First source generation ${event.generation} ended without promotion.${reason} The second queued URL will use normal playback.`,
        `first-end:${event.generation}`,
      );
    } else {
      this.#crossfadeMessage(
        player,
        event.error ? 'Playback failed' : 'Track ended',
        `Second source generation ${event.generation} finished.${reason} Crossfade test ended; entry ${test.entryId}.`,
        `final:${event.generation}`,
      );
    }
    void this.#serialize(player, async () => {
      if (player.crossfadeTest !== test || player.current !== track) return;
      await player.session.clearStage();
      player.current = undefined;
      if (track === test.second || player.queue.length === 0) {
        player.crossfadeTest = undefined;
        player.generation += 1;
      }
      await this.#startNext(player);
    }).catch((error) => {
      if (player.crossfadeTest !== test) return;
      player.lastError = crossfadeError(error);
      this.#crossfadeMessage(player, 'Crossfade test failed', player.lastError);
    });
    return true;
  }

  #bindCrossfadeEvents(player: GuildPlayer): void {
    const session = player.session;
    const prepare = (event: PlaybackEvents['prepareRequested'][0]): void => {
      const test = player.crossfadeTest;
      if (
        !test ||
        !test.allowPreparation ||
        test.firstEnded ||
        player.current !== test.first ||
        event.generation !== test.firstGeneration ||
        event.customMetadata?.track !== test.first ||
        test.attempted.has(event.generation) ||
        !player.queue.includes(test.second)
      )
        return;
      test.attempted.add(event.generation);
      const sourceRevision = ++test.revision;
      test.stage = { generation: event.generation, sourceRevision };
      this.#crossfadeMessage(
        player,
        'Preparation requested',
        `Generation ${event.generation}: remaining ${event.remainingMs} ms; transition in ${event.transitionInMs} ms. Entry ${test.entryId}.`,
      );
      this.#crossfadeMessage(
        player,
        'Second source staging started',
        `Entry ${test.entryId}; expected generation ${event.generation}; queue revision 1; source revision ${sourceRevision}.`,
      );
      // Do not hold the command queue while a staging RPC is in flight. Stop,
      // seek and disconnect remain available; native generation fencing rejects
      // a request whose timeline was invalidated before it reaches the engine.
      void session
        .stageNext(
          { url: test.second.url },
          {
            expectedGeneration: event.generation,
            entryId: test.entryId,
            queueRevision: 1,
            sourceRevision,
            customMetadata: {
              ...player.queueMetadata,
              queueGeneration: player.generation,
              track: test.second,
            },
          },
        )
        .catch((error) => {
          if (
            player.crossfadeTest !== test ||
            test.stage?.generation !== event.generation ||
            !test.allowPreparation
          )
            return;
          this.#crossfadeMessage(
            player,
            'Stage failed',
            `Generation ${event.generation}; entry ${test.entryId}. ${crossfadeError(error)}. Outgoing playback continues.`,
            `stageFailed:${event.generation}:${sourceRevision}`,
          );
        });
    };
    const stage =
      (kind: 'stageReady' | 'stageFailed' | 'stageInvalidated') =>
      (event: PlaybackEvents['stageReady'][0] & { reason?: string }): void => {
        const test = player.crossfadeTest;
        if (
          !test ||
          event.entryId !== test.entryId ||
          event.queueRevision !== 1 ||
          event.generation !== test.stage?.generation ||
          event.sourceRevision !== test.stage.sourceRevision
        )
          return;
        this.#crossfadeMessage(
          player,
          {
            stageReady: 'Stage ready',
            stageFailed: 'Stage failed',
            stageInvalidated: 'Stage invalidated',
          }[kind],
          `Generation ${event.generation}; entry ${event.entryId}; queue revision ${event.queueRevision}; source revision ${event.sourceRevision}.${event.reason ? ` ${crossfadeError(event.reason)}` : ''}`,
          `${kind}:${event.generation}:${event.sourceRevision}`,
        );
      };
    const seek = (event: PlaybackEvents['playbackSeeked'][0]): void => {
      const test = player.crossfadeTest;
      if (!test) return;
      if (event.customMetadata?.track === test.first && player.current === test.first) {
        test.firstGeneration = event.generation;
        test.stage = undefined;
      } else if (event.customMetadata?.track === test.second && player.current === test.second) {
        test.secondGeneration = event.generation;
      }
    };
    const cancelled = (event: PlaybackEvents['transitionCancelled'][0]): void => {
      const test = player.crossfadeTest;
      if (
        !test ||
        event.entryId !== test.entryId ||
        event.failedGeneration !== test.secondGeneration
      )
        return;
      player.current = test.first;
      if (!player.queue.includes(test.second)) player.queue.unshift(test.second);
      test.secondGeneration = undefined;
      test.firstGeneration = event.generation;
      this.#crossfadeMessage(
        player,
        'Transition cancelled',
        `Incoming generation ${event.failedGeneration} abandoned. Outgoing generation ${event.generation} continues; entry ${test.entryId}.`,
        `cancel:${event.failedGeneration}`,
      );
    };
    const ready = stage('stageReady');
    const failed = stage('stageFailed');
    const invalidated = stage('stageInvalidated');
    session.on(PlaybackSessionEvents.prepareRequested, prepare);
    session.on(PlaybackSessionEvents.stageReady, ready);
    session.on(PlaybackSessionEvents.stageFailed, failed);
    session.on(PlaybackSessionEvents.stageInvalidated, invalidated);
    session.on(PlaybackSessionEvents.playbackSeeked, seek);
    session.on(PlaybackSessionEvents.transitionCancelled, cancelled);
    player.lifecycleUnsubscribers.push(
      () => session.off(PlaybackSessionEvents.prepareRequested, prepare),
      () => session.off(PlaybackSessionEvents.stageReady, ready),
      () => session.off(PlaybackSessionEvents.stageFailed, failed),
      () => session.off(PlaybackSessionEvents.stageInvalidated, invalidated),
      () => session.off(PlaybackSessionEvents.playbackSeeked, seek),
      () => session.off(PlaybackSessionEvents.transitionCancelled, cancelled),
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
      .setDescription(sanitizeLifecycleText(description, 4_000))
      .setFooter({ text: 'Automatic playback notification' })
      .setTimestamp();
    if (metadata?.track.requestedBy) {
      embed.addFields({
        name: 'Requested by',
        value: `<@${metadata.track.requestedBy}>`,
        inline: true,
      });
    }
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

function formatLifecyclePosition(positionMs: number): string {
  const seconds = Math.floor(Math.max(0, positionMs) / 1_000);
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  const remainingSeconds = seconds % 60;
  const twoDigits = (value: number): string => String(value).padStart(2, '0');
  return hours > 0
    ? `${hours}:${twoDigits(minutes)}:${twoDigits(remainingSeconds)}`
    : `${minutes}:${twoDigits(remainingSeconds)}`;
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
    loopMode: player.loopMode,
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

function sanitizeLifecycleText(value: string, limit: number): string {
  const clean = stripControlCharacters(value)
    .replace(/@(everyone|here)/gi, '@\u200b$1')
    .trim();
  return clean.length > limit ? `${clean.slice(0, limit - 1)}…` : clean;
}

function crossfadeError(error: unknown): string {
  return safeLifecycleError(
    asError(error).message.replace(/https?:\/\/[^\s<>"']+/gi, '[media URL]'),
  );
}
