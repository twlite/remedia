import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  getVoiceCommandContext,
  withCommandReply,
} from '../voice/command-helpers.js';

export default {
  data: { name: 'skip', description: 'Skip the current track and play the next queued URL.' },
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const playback = VoiceRuntime.current.playback;
      const result = await playback.skipWithContext(guildId, channelId);
      const skipped = `Skipped ${displayTrack(result.skipped)}. `;
      return result.next
        ? `${skipped}Now playing ${displayTrack(result.next)}.`
        : result.queuedNext
          ? `${skipped}Queued ${displayTrack(result.queuedNext)} to start when the voice connection is restored.`
          : `${skipped}The queue is empty.`;
    });
  },
} satisfies Command;
