import assert from 'node:assert/strict';
import { test } from 'node:test';
import { EmbedBuilder, type ChatInputCommandInteraction } from 'discord.js';
import type { DiscordSession, PlaybackState, RemediaClient } from '@remedia/client';
import filterCommand from './filter.js';
import volumeCommand from './volume.js';
import { VoiceRuntime } from '../voice/runtime.js';

test('filter and volume slash commands read and change the guild playback settings', async () => {
  const session = new FakeDiscordSession();
  const remedia = {
    createDiscordSession: () => session as unknown as DiscordSession,
    receive() {},
    close: async () => {},
  } as unknown as RemediaClient;
  const runtime = VoiceRuntime.start(remedia);

  try {
    await runtime.playback.enqueue('guild', 'channel', 'bot', {
      url: 'https://media.example/track.wav',
      requestedBy: 'user',
    });

    let response = await invoke(filterCommand, 'set', 'nightcore');
    assert.equal(response.description, 'Applied the nightcore filter.');
    assert.equal(response.color, 0x5865f2);
    assert.equal(session.filters.graph, 'asetrate=60000,aresample=48000');

    response = await invoke(filterCommand, 'list');
    assert.match(
      response.description ?? '',
      /8d, bassboost, nightcore, vaporwave, tremolo, vibrato/,
    );
    assert.match(response.description ?? '', /Current: nightcore/);
    response = await invoke(filterCommand, 'set', 'not-a-preset');
    assert.match(response.description ?? '', /Unknown filter preset/i);
    assert.equal(response.color, 0xed4245);
    assert.equal(session.filters.graph, 'asetrate=60000,aresample=48000');

    response = await invoke(volumeCommand, undefined, undefined, undefined);
    assert.equal(response.description, 'Current volume: 100%.');
    response = await invoke(volumeCommand, undefined, undefined, 75);
    assert.equal(response.description, 'Volume set to 75%.');
    assert.equal(session.volume, 0.75);

    response = await invoke(filterCommand, 'clear');
    assert.equal(response.description, 'Audio filter cleared.');
    assert.equal(session.filters.graph, null);
    assert.equal(session.volume, 0.75);

    response = await invoke(volumeCommand, undefined, undefined, 201);
    assert.match(response.description ?? '', /integer from 0 to 200 percent/i);
  } finally {
    await runtime.close();
  }
});

async function invoke(
  command: typeof filterCommand | typeof volumeCommand,
  subcommand?: string,
  preset?: string,
  percent?: number,
): Promise<ReturnType<EmbedBuilder['toJSON']>> {
  let response: ReturnType<EmbedBuilder['toJSON']> | undefined;
  const interaction = {
    commandName: command.data.name,
    inGuild: () => true,
    guildId: 'guild',
    guild: { voiceStates: { cache: new Map([['user', { channelId: 'channel' }]]) } },
    user: { id: 'user' },
    client: { user: { id: 'bot' } },
    options: {
      getSubcommand: () => subcommand,
      getString: () => preset,
      getInteger: () => percent ?? null,
    },
    deferReply: async () => {},
    editReply: async ({ embeds }: { embeds: EmbedBuilder[] }) => {
      response = embeds[0]?.toJSON();
    },
  } as unknown as ChatInputCommandInteraction;
  await command.execute(interaction);
  assert.ok(response, 'command should respond with an embed');
  return response;
}

class FakeDiscordSession {
  readonly guildId = 'guild';
  readonly channelId = 'channel';
  readonly userId = 'bot';
  connectionState = 'disconnected';
  state: PlaybackState = 'idle';
  position = {
    mediaMs: null,
    mediaPositionAccuracy: null,
    submittedMs: 0,
    playedMs: null,
  };
  durationMs = 10_000;
  error: string | null = null;
  volume = 1;
  disposed = false;
  readonly filters = {
    graph: null as string | null,
    set: async (graph: string) => {
      this.filters.graph = graph;
    },
    clear: async () => {
      this.filters.graph = null;
    },
  };

  async connect(): Promise<void> {
    this.connectionState = 'connected';
  }

  async play(): Promise<{
    streamIndex: number;
    codec: string;
    sampleRate: number;
    channels: number;
    channelLayout: string;
    durationMs: number;
    bitRate: null;
    metadata: Record<string, string>;
  }> {
    this.state = 'playing';
    return {
      streamIndex: 0,
      codec: 'wav',
      sampleRate: 48_000,
      channels: 2,
      channelLayout: 'stereo',
      durationMs: 10_000,
      bitRate: null,
      metadata: {},
    };
  }

  async setVolume(gain: number): Promise<void> {
    this.volume = gain;
  }

  async stop(): Promise<void> {
    this.state = 'stopped';
  }

  async wait(): Promise<PlaybackState> {
    return this.state;
  }

  async dispose(): Promise<void> {
    this.disposed = true;
  }
}
