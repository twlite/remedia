import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { getVoiceCommandContext, withCommandReply } from '../voice/command-helpers.js';
import { LOOP_MODES, type LoopMode } from '../voice/playback-manager.js';

const LOOP_DESCRIPTIONS: Readonly<Record<LoopMode, string>> = {
  off: 'Looping is off.',
  track: 'The current track will repeat until you change the mode or skip it.',
  queue: 'Completed and skipped tracks will move to the end of the queue.',
};

export default {
  data: {
    name: 'loop',
    description: 'Choose whether this server repeats its track or queue.',
    options: [
      {
        type: 3,
        name: 'mode',
        description: 'Playback loop mode.',
        required: true,
        choices: LOOP_MODES.map((mode) => ({
          name: mode.charAt(0).toUpperCase() + mode.slice(1),
          value: mode,
        })),
      },
    ],
  },
  async execute(interaction) {
    await withCommandReply(
      interaction,
      async () => {
        const { guildId, channelId } = getVoiceCommandContext(interaction);
        const mode = interaction.options.getString('mode', true) as LoopMode;
        await VoiceRuntime.current.playback.setLoopMode(guildId, channelId, mode);
        return LOOP_DESCRIPTIONS[mode];
      },
    );
  },
} satisfies Command;
