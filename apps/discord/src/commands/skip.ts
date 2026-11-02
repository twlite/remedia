import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { displayUrl, getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'skip', description: 'Skip the current track and play the next queued URL.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const next = await VoiceRuntime.current.playback.skip(guildId, channelId);
      return next
        ? `Skipped. Now playing ${displayUrl(next.url)}.`
        : 'Skipped. The queue is empty.';
    });
  },
} satisfies Command;
