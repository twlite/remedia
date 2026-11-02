import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { displayTrack, getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'skip', description: 'Skip the current track and play the next queued URL.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const playback = VoiceRuntime.current.playback;
      const snapshot = await playback.snapshot(guildId, channelId);
      const next = await playback.skip(guildId, channelId);
      const skipped = snapshot.current
        ? `Skipped ${displayTrack(snapshot.current)}. `
        : 'Skipped. ';
      return next
        ? `${skipped}Now playing ${displayTrack(next)}.`
        : `${skipped}The queue is empty.`;
    });
  },
} satisfies Command;
