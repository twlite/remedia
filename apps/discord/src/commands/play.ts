import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  getVoiceCommandContext,
  parseHttpUrl,
  withVoiceReply,
} from '../voice/command-helpers.js';

export default {
  data: {
    name: 'play',
    description: 'Play or queue media from an HTTP or HTTPS URL.',
    options: [
      {
        type: 3,
        name: 'url',
        description: 'Raw media URL supported by the installed FFmpeg libraries.',
        required: true,
      },
    ],
  },
  async execute(interaction) {
    await withVoiceReply(interaction, async () => {
      const context = getVoiceCommandContext(interaction);
      const url = parseHttpUrl(interaction.options.getString('url', true));
      const result = await VoiceRuntime.current.playback.enqueue(
        context.guildId,
        context.channelId,
        context.botUserId,
        {
          url,
          requestedBy: context.requesterId,
        },
      );
      const source = displayTrack(result.track);
      if (result.started) {
        const duration = result.track.info?.durationMs;
        const length =
          duration === undefined || duration === null ? '' : ` (${formatDuration(duration)})`;
        return `Now playing ${source}${length}.`;
      }
      return `Queued #${result.queuePosition}: ${source}.`;
    });
  },
} satisfies Command;

function formatDuration(milliseconds: number): string {
  const totalSeconds = Math.floor(milliseconds / 1_000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, '0')}`;
}
