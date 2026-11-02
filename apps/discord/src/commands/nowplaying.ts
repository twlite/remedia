import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  formatDuration,
  getVoiceCommandContext,
  withVoiceReply,
} from '../voice/command-helpers.js';

export default {
  data: { name: 'nowplaying', description: 'Show the current track and playback position.' },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const snapshot = await VoiceRuntime.current.playback.snapshot(guildId, channelId);
      if (!snapshot.current) {
        if (snapshot.lastError) return `The last track failed: ${snapshot.lastError}`;
        return 'Nothing is playing.';
      }

      const label = displayTrack(snapshot.current);
      const duration = snapshot.durationMs ?? snapshot.current.info?.durationMs ?? null;
      const position = `${snapshot.positionEstimated ? '~' : ''}${formatDuration(snapshot.positionMs)}`;
      const failure = snapshot.lastError ? ` Last track error: ${snapshot.lastError}` : '';
      return `Now playing **${label}** — position ${position} / ${formatDuration(duration)} (${snapshot.state}).${failure}`;
    });
  },
} satisfies Command;
