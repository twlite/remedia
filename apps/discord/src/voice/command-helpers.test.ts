import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { ChatInputCommandInteraction } from 'discord.js';
import { safeText, withCommandReply } from './command-helpers.js';

test('successful commands are public and include the invoking user', async () => {
  const calls: Array<{ method: string; value?: unknown }> = [];
  const interaction = fakeInteraction(calls);

  await withCommandReply(interaction, async () => 'Paused Example Song.');

  assert.deepEqual(
    calls.map(({ method }) => method),
    ['deferReply', 'followUp'],
  );
  assert.deepEqual(calls[0]?.value, { flags: 64 });
  const followUp = calls[1]?.value as {
    ephemeral: boolean;
    embeds: Array<{ data: { fields?: Array<{ name: string; value: string }> } }>;
  };
  assert.equal(followUp.ephemeral, false);
  assert.deepEqual(followUp.embeds[0]?.data.fields, [
    { name: 'Action by', value: '<@123456789012345678>', inline: true },
  ]);
});

test('a failed public confirmation remains an operational failure, not a failed command', async () => {
  const calls: Array<{ method: string; value?: unknown }> = [];
  const interaction = fakeInteraction(calls, { failPublicFollowUp: true });

  await withCommandReply(interaction, async () => 'Playback was paused.');

  assert.deepEqual(
    calls.map(({ method }) => method),
    ['deferReply', 'followUp', 'editReply'],
  );
  const fallback = calls[2]?.value as {
    embeds: Array<{ data: { title?: string } }>;
  };
  assert.equal(fallback.embeds[0]?.data.title, 'Confirmation could not be sent');
});

test('command failures stay ephemeral and are sanitized', async () => {
  const calls: Array<{ method: string; value?: unknown }> = [];
  const interaction = fakeInteraction(calls);

  await withCommandReply(interaction, async () => {
    throw new Error('bad input https://user:secret@example.test/file.mp3?token=private @everyone');
  });

  assert.deepEqual(
    calls.map(({ method }) => method),
    ['deferReply', 'editReply'],
  );
  const response = calls[1]?.value as {
    embeds: Array<{
      data: {
        title?: string;
        description?: string;
        fields?: Array<{ name: string; value: string }>;
      };
    }>;
  };
  assert.equal(response.embeds[0]?.data.title, 'Command failed');
  assert.doesNotMatch(response.embeds[0]?.data.description ?? '', /secret|private|@everyone/);
  assert.deepEqual(response.embeds[0]?.data.fields, [
    { name: 'Action by', value: '<@123456789012345678>', inline: true },
  ]);
});

test('plain text sanitizer strips controls, mentions, and URL credentials', () => {
  assert.equal(
    safeText('line\nhttps://user:pass@example.test/song?token=secret @here', 200),
    'line https://example.test/song @\u200bhere',
  );
});

function fakeInteraction(
  calls: Array<{ method: string; value?: unknown }>,
  options: { failPublicFollowUp?: boolean } = {},
): ChatInputCommandInteraction {
  let followUpCount = 0;
  const interaction = {
    commandName: 'pause',
    user: { id: '123456789012345678' },
    deferReply: async (value: unknown) => calls.push({ method: 'deferReply', value }),
    deleteReply: async () => calls.push({ method: 'deleteReply' }),
    editReply: async (value: unknown) => calls.push({ method: 'editReply', value }),
    followUp: async (value: unknown) => {
      calls.push({ method: 'followUp', value });
      followUpCount += 1;
      if (options.failPublicFollowUp && followUpCount === 1) throw new Error('Discord failed');
    },
  };
  return interaction as unknown as ChatInputCommandInteraction;
}
