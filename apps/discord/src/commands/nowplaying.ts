import type { Command } from './index.js';
import { EmbedBuilder } from 'discord.js';
import { VoiceRuntime } from '../voice/runtime.js';
import {
  displayTrack,
  formatDuration,
  getVoiceCommandContext,
  withCommandReply,
} from '../voice/command-helpers.js';
import type { PlaybackSnapshot } from '../voice/playback-manager.js';

export default {
  data: { name: 'nowplaying', description: 'Show the current track and playback position.' },
  async execute(interaction) {
    await withCommandReply(interaction, async () => {
      const { guildId, channelId } = getVoiceCommandContext(interaction);
      const snapshot = await VoiceRuntime.current.playback.snapshot(guildId, channelId);
      if (!snapshot.current) {
        if (snapshot.lastError) return `The last track failed: ${snapshot.lastError}`;
        return 'Nothing is playing.';
      }
      return createNowPlayingEmbed(snapshot);
    });
  },
} satisfies Command;

export function makeProgressBar(positionMs: number, durationMs: number, width = 18): string {
  const boundedWidth = Math.max(1, Math.floor(width));
  const ratio = Math.min(1, Math.max(0, positionMs / durationMs));
  const marker = Math.min(boundedWidth - 1, Math.floor(ratio * boundedWidth));
  return Array.from({ length: boundedWidth }, (_, index) => {
    if (index === marker) return '●';
    return index < marker ? '▰' : '▱';
  }).join('');
}

export function createNowPlayingEmbed(snapshot: PlaybackSnapshot): EmbedBuilder {
  if (!snapshot.current) throw new Error('A current track is required to create this embed.');
  const stateColor =
    snapshot.state === 'playing' ? 0x57f287 : snapshot.state === 'paused' ? 0xfee75c : 0x5865f2;
  const duration = snapshot.durationMs ?? snapshot.current.info?.durationMs ?? null;
  const position = `${snapshot.positionEstimated ? '~' : ''}${formatDuration(snapshot.positionMs)}`;
  const bar =
    duration !== null && duration > 0
      ? `\`${makeProgressBar(snapshot.positionMs, duration)}\`\n`
      : '';
  const embed = new EmbedBuilder()
    .setColor(stateColor)
    .setAuthor({ name: 'Now playing' })
    .setTitle(displayTrack(snapshot.current))
    .setDescription(`${bar}**${position}** / **${formatDuration(duration)}**`)
    .addFields(
      { name: 'Status', value: capitalize(snapshot.state), inline: true },
      { name: 'Filter', value: snapshot.filterPresets.join(', ') || 'Off', inline: true },
      { name: 'Volume', value: `${snapshot.volumePercent}%`, inline: true },
    )
    .setFooter({
      text: snapshot.positionEstimated ? 'Source position is estimated' : 'Source media position',
    })
    .setTimestamp();
  if (snapshot.lastError) {
    embed.addFields({ name: 'Playback issue', value: snapshot.lastError.slice(0, 1_024) });
  }
  return embed;
}

function capitalize(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
