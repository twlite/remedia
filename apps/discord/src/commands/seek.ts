import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  getVoiceCommandContext,
  withCommandReply,
} from '../voice/command-helpers.js';

export default {
  data: {
    name: 'seek',
    description: 'Seek within the current track.',
    options: [
      {
        type: 10,
        name: 'seconds',
        description: 'Position in seconds from the start of the track.',
        required: true,
        min_value: 0,
      },
    ],
  },
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const seconds = interaction.options.getNumber('seconds', true);
      const playback = VoiceRuntime.current.playback;
      const snapshot = await playback.snapshot(guildId, channelId);
      await playback.seek(guildId, channelId, seconds * 1_000);
      const track = snapshot.current ? ` in ${displayTrack(snapshot.current)}` : '';
      return `Seeked${track} to ${formatPosition(seconds)}.`;
    });
  },
} satisfies Command;

function formatPosition(seconds: number): string {
  const totalSeconds = Math.floor(seconds);
  const minutes = Math.floor(totalSeconds / 60);
  const remainder = totalSeconds % 60;
  return `${minutes}:${remainder.toString().padStart(2, '0')}`;
}
