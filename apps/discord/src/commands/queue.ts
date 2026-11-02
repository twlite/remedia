import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import { displayTrack, getVoiceCommandContext, withVoiceReply } from '../voice/command-helpers.js';

export default {
  data: {
    name: 'queue',
    description: 'View or clear upcoming tracks.',
    options: [
      { type: 1, name: 'list', description: 'Show the current track and upcoming queue.' },
      { type: 1, name: 'clear', description: 'Remove all upcoming tracks.' },
    ],
  },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const manager = VoiceRuntime.current.playback;
      const action = interaction.options.getSubcommand();
      if (action === 'clear') {
        const removed = await manager.clearQueue(guildId, channelId);
        return removed === 0
          ? 'The queue was already empty.'
          : `Removed ${removed} queued track(s).`;
      }

      const snapshot = await manager.snapshot(guildId, channelId);
      const rows: string[] = [];
      if (snapshot.current) rows.push(`Now: ${displayTrack(snapshot.current)}`);
      for (const [index, track] of snapshot.queue.slice(0, 10).entries()) {
        rows.push(`${index + 1}. ${displayTrack(track)}`);
      }
      if (snapshot.queue.length > 10) rows.push(`…and ${snapshot.queue.length - 10} more.`);
      return rows.length > 0 ? rows.join('\n') : 'The queue is empty.';
    });
  },
} satisfies Command;
