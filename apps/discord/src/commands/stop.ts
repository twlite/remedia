import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  getVoiceCommandContext,
  withCommandReply,
} from '../voice/command-helpers.js';

export default {
  data: { name: 'stop', description: 'Stop playback and clear the queue.' },
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const playback = VoiceRuntime.current.playback;
      const result = await playback.stopWithContext(guildId, channelId);
      const track = result.current ? `Stopped ${displayTrack(result.current)}. ` : '';
      return `${track}Playback stopped and the queue cleared. The bot remains connected.`;
    });
  },
} satisfies Command;
