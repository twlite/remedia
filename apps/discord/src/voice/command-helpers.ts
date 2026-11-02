import type { ChatInputCommandInteraction } from 'discord.js';

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

export async function withVoiceReply(
  interaction: ChatInputCommandInteraction,
  action: () => Promise<string>,
): Promise<void> {
  await interaction.deferReply({ ephemeral: true });
  try {
    await interaction.editReply({ content: await action(), allowedMentions: { parse: [] } });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await interaction.editReply({
      content: `Remedia: ${message}`,
      allowedMentions: { parse: [] },
    });
  }
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
