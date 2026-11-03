import {
  EmbedBuilder,
  MessageFlags,
  type ChatInputCommandInteraction,
  type SendableChannels,
  type TextBasedChannel,
} from 'discord.js';
import type { AudioStreamInfo } from '@remedia/client';

const REMEDIA_BLURPLE = 0x5865f2;
const REMEDIA_RED = 0xed4245;

const COMMAND_TITLES: Readonly<Record<string, string>> = {
  disconnect: 'Voice disconnected',
  filter: 'Audio filter',
  loop: 'Loop mode',
  pause: 'Playback paused',
  ping: 'Remedia is online',
  play: 'Playback',
  queue: 'Playback queue',
  resume: 'Playback resumed',
  seek: 'Playback position',
  skip: 'Track skipped',
  stop: 'Playback stopped',
  volume: 'Master volume',
};

export type CommandResponse = string | EmbedBuilder;

export interface VoiceCommandContext {
  guildId: string;
  channelId: string;
  botUserId: string;
  requesterId: string;
  textChannel: TextBasedChannel & SendableChannels;
}

export function getVoiceCommandContext(
  interaction: ChatInputCommandInteraction,
): VoiceCommandContext {
  if (!interaction.inGuild() || !interaction.guild) {
    throw new Error('This command can only be used in a server.');
  }

  const voiceState = interaction.guild.voiceStates.cache.get(interaction.user.id);
  if (!voiceState?.channelId) throw new Error('Join a voice channel first.');

  const botUserId = interaction.client.user?.id;
  if (!botUserId) throw new Error('The bot is not ready yet. Try again shortly.');
  const textChannel = interaction.channel;
  if (!textChannel?.isTextBased() || !textChannel.isSendable()) {
    throw new Error('Use this command in a text channel where the bot can send messages.');
  }

  return {
    guildId: interaction.guildId,
    channelId: voiceState.channelId,
    botUserId,
    requesterId: interaction.user.id,
    textChannel,
  };
}

export async function withCommandReply(
  interaction: ChatInputCommandInteraction,
  action: () => Promise<CommandResponse>,
): Promise<void> {
  // Defer privately so validation and execution errors stay private. Discord cannot change a
  // deferred reply's visibility, so successful actions delete the private defer and post a public
  // follow-up instead.
  await interaction.deferReply({ flags: MessageFlags.Ephemeral });
  let response: CommandResponse;
  try {
    response = await action();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const reply = {
      embeds: [createErrorEmbed(interaction, message)],
      allowedMentions: { parse: [] as const },
    };
    try {
      await interaction.editReply(reply);
    } catch {
      await interaction.followUp({ ...reply, ephemeral: true }).catch(() => undefined);
    }
    return;
  }

  const embed =
    response instanceof EmbedBuilder ? response : createCommandEmbed(interaction, response);
  if (!embed.data.fields?.some(({ name }) => name === 'Action by')) {
    embed.addFields({ name: 'Action by', value: `<@${interaction.user.id}>`, inline: true });
  }
  try {
    await interaction.followUp({
      embeds: [embed],
      ephemeral: false,
      allowedMentions: { parse: [] },
    });
  } catch {
    const reply = {
      embeds: [createDeliveryFailureEmbed(interaction)],
      allowedMentions: { parse: [] as const },
    };
    try {
      await interaction.editReply(reply);
    } catch {
      await interaction.followUp({ ...reply, ephemeral: true }).catch(() => undefined);
    }
    return;
  }
}

export function createCommandEmbed(
  interaction: ChatInputCommandInteraction,
  description: string,
): EmbedBuilder {
  const command = interaction.commandName;
  return new EmbedBuilder()
    .setColor(REMEDIA_BLURPLE)
    .setAuthor({ name: 'Remedia' })
    .setTitle(COMMAND_TITLES[command] ?? `/${command}`)
    .setDescription(safeText(description, 4_000))
    .setFooter({ text: `/${command}` })
    .setTimestamp();
}

export function createErrorEmbed(
  interaction: ChatInputCommandInteraction,
  message: string,
): EmbedBuilder {
  return new EmbedBuilder()
    .setColor(REMEDIA_RED)
    .setAuthor({ name: 'Remedia' })
    .setTitle('Command failed')
    .setDescription(safeText(message, 3_800))
    .addFields({ name: 'Action by', value: `<@${interaction.user.id}>`, inline: true })
    .setFooter({ text: `/${interaction.commandName}` })
    .setTimestamp();
}

function createDeliveryFailureEmbed(interaction: ChatInputCommandInteraction): EmbedBuilder {
  return new EmbedBuilder()
    .setColor(REMEDIA_RED)
    .setAuthor({ name: 'Remedia' })
    .setTitle('Confirmation could not be sent')
    .setDescription(
      'The command completed, but Discord did not accept its public confirmation. Check the current playback state before retrying.',
    )
    .addFields({ name: 'Action by', value: `<@${interaction.user.id}>`, inline: true })
    .setFooter({ text: `/${interaction.commandName}` })
    .setTimestamp();
}

export function parseHttpUrl(input: string): string {
  const value = input.trim();
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new TypeError('Enter a complete HTTP or HTTPS media URL.');
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new TypeError('Only HTTP and HTTPS media URLs are supported.');
  }
  return value;
}

export function displayUrl(input: string): string {
  const url = new URL(input);
  const display = `${url.protocol}//${url.host}${url.pathname}`;
  return display.length > 160 ? `${display.slice(0, 157)}...` : display;
}

export function displayTrack(track: {
  url: string;
  info?: Pick<AudioStreamInfo, 'metadata'> | undefined;
}): string {
  const title = Object.entries(track.info?.metadata ?? {}).find(
    ([key, value]) => key.toLowerCase() === 'title' && value.trim().length > 0,
  )?.[1];
  const label = title?.trim() || displayUrl(track.url);
  return safeText(label, 160);
}

export function formatDuration(milliseconds: number | null): string {
  if (milliseconds === null || !Number.isFinite(milliseconds) || milliseconds < 0) return 'unknown';
  const seconds = Math.floor(milliseconds / 1_000);
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  const remainder = seconds % 60;
  return hours > 0
    ? `${hours}:${minutes.toString().padStart(2, '0')}:${remainder.toString().padStart(2, '0')}`
    : `${minutes}:${remainder.toString().padStart(2, '0')}`;
}

export function safeText(value: string, maximumLength: number): string {
  const sanitized = stripControlCharacters(value)
    .replace(/@(everyone|here)/gi, '@\u200b$1')
    .replace(/\b(token|password|secret|authorization)(\s*[:=]\s*)[^\s,;&]+/gi, '$1$2[redacted]')
    .replace(/(https?:\/\/)[^\s]+/gi, (candidate) => {
      try {
        const url = new URL(candidate);
        return `${url.protocol}//${url.host}${url.pathname}`;
      } catch {
        return '[link]';
      }
    })
    .trim();
  return sanitized.length > maximumLength
    ? `${sanitized.slice(0, Math.max(0, maximumLength - 1))}…`
    : sanitized || 'No details were provided.';
}

export function stripControlCharacters(value: string): string {
  return Array.from(value, (character) => {
    const code = character.charCodeAt(0);
    return code < 0x20 || code === 0x7f ? ' ' : character;
  }).join('');
}
