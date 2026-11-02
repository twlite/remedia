import { URL } from 'node:url';
import { Events } from 'discord.js';
import { loadCommands } from '../util/loaders.js';
import type { Event } from './index.js';
import { withCommandReply } from '../voice/command-helpers.js';

const commands = await loadCommands(new URL('../commands/', import.meta.url));

export default {
  name: Events.InteractionCreate,
  async execute(interaction) {
    if (interaction.isChatInputCommand()) {
      const command = commands.get(interaction.commandName);

      if (!command) {
        await withCommandReply(interaction, async () => {
          throw new Error(`Command '${interaction.commandName}' not found.`);
        });
        return;
      }

      await command.execute(interaction);
    }
  },
} satisfies Event<Events.InteractionCreate>;
