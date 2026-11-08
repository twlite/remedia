import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { RemediaClient } from '@remedia/client';
import type { ChatInputCommandInteraction, EmbedBuilder } from 'discord.js';
import command from './crossfade.js';
import type { DiscordPlaybackManager } from '../voice/playback-manager.js';
import { VoiceRuntime } from '../voice/runtime.js';

test('crossfade requires both URLs and delegates their validated inputs and invoking channel', async () => {
  assert.deepEqual(
    command.data.options.map((option) => [option.name, option.required]),
    [
      ['first', true],
      ['second', true],
    ],
  );
  const runtime = VoiceRuntime.start({ close: async () => {} } as unknown as RemediaClient);
  let call: Parameters<DiscordPlaybackManager['crossfade']> | undefined;
  runtime.playback.crossfade = async (...args) => {
    call = args;
    return 'remote-node';
  };
  try {
    const h = interaction(
      ' https://example.test/a.mp3?signature=private ',
      'http://example.test/b.mp3',
    );
    await command.execute(h.value);
    assert.deepEqual(call?.slice(0, 5), [
      'guild',
      'voice',
      'bot',
      { url: 'https://example.test/a.mp3?signature=private', requestedBy: 'user' },
      { url: 'http://example.test/b.mp3', requestedBy: 'user' },
    ]);
    assert.equal(call?.[5], h.channel);
    assert.match(h.description(), /remote-node/);
    assert.ok(!h.description().includes('private'));
    assert.equal(h.wasError(), false);
  } finally {
    await runtime.close();
  }
});

for (const [first, second] of [
  ['file:///first.wav', 'https://example.test/b.mp3'],
  ['https://example.test/a.mp3', 'not-a-url'],
]) {
  test(`rejects invalid first or second input before playback: ${first} / ${second}`, async () => {
    const runtime = VoiceRuntime.start({ close: async () => {} } as unknown as RemediaClient);
    let calls = 0;
    runtime.playback.crossfade = async () => {
      calls++;
      return 'node';
    };
    try {
      const h = interaction(first!, second!);
      await command.execute(h.value);
      assert.equal(calls, 0);
      assert.match(h.description(), /HTTP|HTTPS/);
      assert.equal(h.wasError(), true);
    } finally {
      await runtime.close();
    }
  });
}

function interaction(first: string, second: string) {
  let response: { embeds: EmbedBuilder[] } | undefined;
  let error = false;
  const channel = { isTextBased: () => true, isSendable: () => true, send: async () => {} };
  const value = {
    commandName: 'crossfade',
    guildId: 'guild',
    inGuild: () => true,
    guild: { voiceStates: { cache: new Map([['user', { channelId: 'voice' }]]) } },
    user: { id: 'user' },
    client: { user: { id: 'bot' } },
    channel,
    options: {
      getString: (name: string, required: boolean) => {
        assert.equal(required, true);
        return name === 'first' ? first : second;
      },
    },
    deferReply: async () => {},
    editReply: async (reply: { embeds: EmbedBuilder[] }) => {
      response = reply;
      error = true;
    },
    followUp: async (reply: { embeds: EmbedBuilder[] }) => {
      response = reply;
    },
  } as unknown as ChatInputCommandInteraction;
  return {
    value,
    channel,
    description: () => response?.embeds[0]?.data.description ?? '',
    wasError: () => error,
  };
}
