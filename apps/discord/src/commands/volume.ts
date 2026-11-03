import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withCommandReply } from '../voice/command-helpers.js';

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
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const manager = VoiceRuntime.current.playback;
      const percent = interaction.options.getInteger('percent');
      if (percent === null) {
        const snapshot = await manager.snapshot(guildId, channelId);
        return `Current volume: ${snapshot.volumePercent}%.`;
      }
      const { previousPercent } = await manager.setVolumeWithContext(guildId, channelId, percent);
      if (previousPercent === percent) return `Volume remains at ${percent}%.`;
      return percent > 100
        ? `Volume changed from ${previousPercent}% to ${percent}%. Values above 100% may clip.`
        : `Volume changed from ${previousPercent}% to ${percent}%.`;
    });
  },
} satisfies Command;
