import type { Command } from './index.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  getVoiceCommandContext,
  parseHttpUrl,
  withCommandReply,
} from '../voice/command-helpers.js';

export default {
  data: {
    name: 'crossfade',
    description: 'Smoke-test automatic crossfade between two direct media URLs.',
    options: [
      { type: 3, name: 'first', description: 'First HTTP/HTTPS media URL.', required: true },
      { type: 3, name: 'second', description: 'Second HTTP/HTTPS media URL.', required: true },
    ],
  },
  /**
   * Validate direct URLs and delegate session ownership to the playback manager.
   * @param interaction - Guild slash-command invocation with both required URLs.
   * @returns Resolves after the command reply; reports validation failures privately.
   */
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const context = getVoiceCommandContext(interaction);
      const first = parseHttpUrl(interaction.options.getString('first', true));
      const second = parseHttpUrl(interaction.options.getString('second', true));
      const node = await VoiceRuntime.current.playback.crossfade(
        context.guildId,
        context.channelId,
        context.botUserId,
        { url: first, requestedBy: context.requesterId },
        { url: second, requestedBy: context.requesterId },
        context.textChannel,
      );
      return `Crossfade test started on node ${node}. Lifecycle diagnostics will appear in this channel. Use /disconnect before starting another test or ordinary /play queue.`;
    });
  },
} satisfies Command;
