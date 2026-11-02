import type { Command } from './index.js';
import { withCommandReply } from '../voice/command-helpers.js';

export default {
  data: {
    name: 'ping',
    description: 'Ping!',
  },
  async execute(interaction) {
    await withCommandReply(interaction, async () => 'Pong!');
  },
} satisfies Command;
