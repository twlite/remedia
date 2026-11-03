import process from 'node:process';
import { URL } from 'node:url';
import { Client, Events, GatewayIntentBits, type Client as DiscordClient } from 'discord.js';
import {
  RemediaClient,
  type DiscordGatewayVoicePayload,
  type RemediaNodeConfig,
} from '@remedia/client';
import { loadEvents } from './util/loaders.js';
import { VoiceRuntime } from './voice/runtime.js';
import { remoteNodesFromEnvironment } from './voice/node-config.js';

const token = process.env.DISCORD_TOKEN;
if (!token) throw new Error('DISCORD_TOKEN must be set in apps/discord/.env.');

process.title = 'Remedia Discord Bot';

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates],
});
const mode = process.env.REMEDIA_MODE ?? 'embedded';
if (mode !== 'embedded' && mode !== 'remote') {
  throw new Error('REMEDIA_MODE must be either "embedded" or "remote".');
}
const nodes: RemediaNodeConfig[] =
  mode === 'embedded'
    ? [{ type: 'embedded', id: 'embedded' }]
    : remoteNodesFromEnvironment(process.env.REMEDIA_NODES);
let runtime: VoiceRuntime | undefined;
let resolveReady!: (readyClient: DiscordClient<true>) => void;
const ready = new Promise<DiscordClient<true>>((resolve) => {
  resolveReady = resolve;
});
client.once(Events.ClientReady, resolveReady);
client.on(Events.Raw, (packet) => runtime?.receive(packet));

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
  shutdown = (runtime?.close() ?? Promise.resolve()).finally(() => {
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
  const readyClient = await ready;
  runtime = VoiceRuntime.start(
    await RemediaClient.create({
      nodes,
      botUserId: readyClient.user.id,
      send(guildId, payload) {
        sendGatewayVoicePayload(client, guildId, payload);
      },
    }),
  );
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
