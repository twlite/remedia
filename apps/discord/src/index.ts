import process from 'node:process';
import { URL } from 'node:url';
import { Client, Events, GatewayIntentBits, type Client as DiscordClient } from 'discord.js';
import { Remedia, type DiscordGatewayVoicePayload } from '@remedia/node';
import { loadEvents } from './util/loaders.js';
import { VoiceRuntime } from './voice/runtime.js';

const token = process.env.DISCORD_TOKEN;
if (!token) throw new Error('DISCORD_TOKEN must be set in apps/discord/.env.');

process.title = 'Remedia Discord Bot';

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates],
});
const runtime = VoiceRuntime.start(
  await Remedia.create({
    send(guildId, payload) {
      sendGatewayVoicePayload(client, guildId, payload);
    },
  }),
);

client.on(Events.Raw, (packet) => runtime.receive(packet));

// Load the events and commands
const events = await loadEvents(new URL('events/', import.meta.url));

// Register the event handlers
for (const event of events) {
  client[event.once ? 'once' : 'on'](event.name, async (...args) => {
    try {
      await event.execute(...args);
    } catch (error) {
      console.error(`Error executing event ${String(event.name)}:`, error);
    }
  });
}

let shutdown: Promise<void> | undefined;
async function close(): Promise<void> {
  if (shutdown) return shutdown;
  shutdown = runtime.close().finally(() => {
    client.destroy();
  });
  return shutdown;
}

process.once('SIGINT', () => {
  void close().catch((error: unknown) => console.error('Shutdown failed:', error));
});
process.once('SIGTERM', () => {
  void close().catch((error: unknown) => console.error('Shutdown failed:', error));
});

try {
  await client.login(token);
} catch (error) {
  await close().catch((closeError: unknown) => {
    console.error('Cleanup after Gateway login failure also failed:', closeError);
  });
  throw error;
}

function sendGatewayVoicePayload(
  gatewayClient: DiscordClient,
  guildId: string,
  payload: DiscordGatewayVoicePayload,
): void {
  const maybeGuild = gatewayClient.guilds.cache.get(guildId);
  if (maybeGuild?.shard) {
    maybeGuild.shard.send(payload);
    return;
  }

  const shardCount = gatewayClient.ws.shards.size;
  if (shardCount === 0) throw new Error('Discord Gateway has no active shards.');
  const shardId = Number((BigInt(guildId) >> 22n) % BigInt(shardCount));
  const shard = gatewayClient.ws.shards.get(shardId);
  if (!shard) throw new Error(`Discord Gateway shard ${shardId} is not connected.`);
  shard.send(payload);
}
