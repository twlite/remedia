import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  getVoiceCommandContext,
  withCommandReply,
} from '../voice/command-helpers.js';

export default {
  data: { name: 'resume', description: 'Resume the paused track.' },
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const playback = VoiceRuntime.current.playback;
      const result = await playback.resumeWithContext(guildId, channelId);
      const track = result.current ? ` Track: ${displayTrack(result.current)}.` : '';
      return result.changed
        ? `Playback changed from ${displayState(result.previousState)} to Playing.${track}`
        : `Playback is already playing.${track}`;
    });
  },
} satisfies Command;

function displayState(state: string): string {
  return state.charAt(0).toUpperCase() + state.slice(1);
}
