import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { FILTER_PRESET_NAMES, type FilterPresetName } from '../voice/filter-presets.js';
import { getVoiceCommandContext, withCommandReply } from '../voice/command-helpers.js';

export default {
  data: {
    name: 'filter',
    description: 'Set, clear, or list this server’s audio filters.',
    options: [
      {
        type: 1,
        name: 'set',
        description: 'Toggle an audio filter preset on or off.',
        options: [
          {
            type: 3,
            name: 'preset',
            description: 'Filter preset to apply.',
            required: true,
            choices: FILTER_PRESET_NAMES.map((name) => ({ name, value: name })),
          },
        ],
      },
      { type: 1, name: 'clear', description: 'Disable all active filters.' },
      { type: 1, name: 'list', description: 'Show available presets and active filters.' },
    ],
  },
  async execute(interaction) {
    await withCommandReply(
      interaction,
      async () => {
        const { guildId, channelId } = getVoiceCommandContext(interaction);
        const manager = VoiceRuntime.current.playback;
        const action = interaction.options.getSubcommand();
        if (action === 'set') {
          const preset = interaction.options.getString('preset', true) as FilterPresetName;
          const result = await manager.toggleFilter(guildId, channelId, preset);
          const snapshot = await manager.snapshot(guildId, channelId);
          const active = snapshot.filterPresets.join(', ') || 'none';
          const status = result === 'added' ? 'Added' : 'Removed';
          return `${status} the ${preset} filter. Active filters: ${active}.`;
        }
        if (action === 'clear') {
          await manager.clearFilters(guildId, channelId);
          return 'All audio filters cleared.';
        }

        const snapshot = await manager.snapshot(guildId, channelId);
        const selected = snapshot.filterPresets.join(', ') || 'none';
        return `Available filters: ${FILTER_PRESET_NAMES.join(', ')}. Current: ${selected}.`;
      },
    );
  },
} satisfies Command;
