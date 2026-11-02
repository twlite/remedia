import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'stop', description: 'Stop playback and clear the queue.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      await VoiceRuntime.current.playback.stop(guildId, channelId);
      return 'Playback stopped and the queue cleared. The bot remains connected.';
    });
  },
} satisfies Command;
