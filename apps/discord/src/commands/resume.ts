import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'resume', description: 'Resume the paused track.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      await VoiceRuntime.current.playback.resume(guildId, channelId);
      return 'Playback resumed.';
    });
  },
} satisfies Command;
