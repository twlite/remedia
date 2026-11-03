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
    await withCommandReply(
      interaction,
      async () => {
        const { guildId, channelId } = getVoiceCommandContext(interaction);
        const playback = VoiceRuntime.current.playback;
        const snapshot = await playback.snapshot(guildId, channelId);
        await playback.resume(guildId, channelId);
        return snapshot.current
          ? `Resumed ${displayTrack(snapshot.current)}.`
          : 'Playback resumed.';
      },
    );
  },
} satisfies Command;
