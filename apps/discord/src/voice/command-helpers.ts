import { EmbedBuilder, MessageFlags, type ChatInputCommandInteraction } from 'discord.js';
import type { AudioStreamInfo } from '@remedia/node';

const REMEDIA_BLURPLE = 0x5865f2;
const REMEDIA_RED = 0xed4245;

const COMMAND_TITLES: Readonly<Record<string, string>> = {
  disconnect: 'Voice disconnected',
  filter: 'Audio filter',
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

  return {
    guildId: interaction.guildId,
    channelId: voiceState.channelId,
    botUserId,
    requesterId: interaction.user.id,
  };
}

export async function withCommandReply(
  interaction: ChatInputCommandInteraction,
  action: () => Promise<CommandResponse>,
  ephemeral = false,
): Promise<void> {
  await interaction.deferReply(ephemeral ? { flags: MessageFlags.Ephemeral } : {});
  try {
    const response = await action();
    await interaction.editReply({
      embeds: [
        response instanceof EmbedBuilder ? response : createCommandEmbed(interaction, response),
      ],
      allowedMentions: { parse: [] },
    });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await interaction.editReply({
      embeds: [createErrorEmbed(interaction, message)],
      allowedMentions: { parse: [] },
    });
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
    .setDescription(description)
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
    .setDescription(message)
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
  const display = `${url.origin}${url.pathname}`;
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
  return label.length > 160 ? `${label.slice(0, 157)}...` : label;
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
