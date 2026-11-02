import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'pause', description: 'Pause the current track.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      await VoiceRuntime.current.playback.pause(guildId, channelId);
      return 'Playback paused.';
    });
  },
} satisfies Command;
