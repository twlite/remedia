import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: {
    name: 'volume',
    description: 'Read or set this server’s playback volume.',
    options: [
      {
        type: 4,
        name: 'percent',
        description: 'Linear volume percentage from 0 to 200.',
        required: false,
        min_value: 0,
        max_value: 200,
      },
    ],
  },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const manager = VoiceRuntime.current.playback;
      const percent = interaction.options.getInteger('percent');
      if (percent === null) {
        const snapshot = await manager.snapshot(guildId, channelId);
        return `Current volume: ${snapshot.volumePercent}%.`;
      }
      await manager.setVolume(guildId, channelId, percent);
      return percent > 100
        ? `Volume set to ${percent}%. Values above 100% may clip.`
        : `Volume set to ${percent}%.`;
    });
  },
} satisfies Command;
