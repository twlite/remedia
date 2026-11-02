import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withCommandReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'disconnect', description: 'Stop playback and leave the voice channel.' },
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const disconnected = await VoiceRuntime.current.playback.disconnect(guildId, channelId);
      return disconnected ? 'Disconnected from the voice channel.' : 'No voice session was active.';
    });
  },
} satisfies Command;
