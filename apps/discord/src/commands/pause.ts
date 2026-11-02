import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { displayTrack, getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: { name: 'pause', description: 'Pause the current track.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const playback = VoiceRuntime.current.playback;
      const snapshot = await playback.snapshot(guildId, channelId);
      await playback.pause(guildId, channelId);
      return snapshot.current ? `Paused ${displayTrack(snapshot.current)}.` : 'Playback paused.';
    });
  },
} satisfies Command;
