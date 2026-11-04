import process from 'node:process';
import { URL } from 'node:url';
import { RemediaClient } from '@remedia/client';
import {
  Client,
  Events,
  GatewayIntentBits,
  calculateShardId,
  type Client as DiscordClient,
} from 'discord.js';

import { loadEvents } from './util/loaders.js';
import { VoiceRuntime } from './voice/runtime.js';

const token = process.env.DISCORD_TOKEN;
if (!token) throw new Error('DISCORD_TOKEN must be set in apps/discord/.env.');

process.title = 'Remedia Discord Bot';

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates],
});

let runtime: VoiceRuntime | undefined;
let shutdown: Promise<void> | undefined;

const ready = new Promise<DiscordClient<true>>((resolve) => {
  client.once(Events.ClientReady, resolve);
});

client.on(Events.Raw, (packet) => runtime?.receive(packet));

const events = await loadEvents(new URL('events/', import.meta.url));

for (const event of events) {
  client[event.once ? 'once' : 'on'](event.name, async (...args) => {
    try {
      await event.execute(...args);
    } catch (error) {
      console.error(`Error executing event ${String(event.name)}:`, error);
    }
  });
}

function sendGatewayPayload(
  guildId: string,
  payload: Parameters<NonNullable<Parameters<typeof RemediaClient.create>[0]['send']>>[1],
): void {
  const guild = client.guilds.cache.get(guildId);

  if (guild?.shard) {
    guild.shard.send(payload);
    return;
  }

  const shardCount = client.ws.shards.size;
  if (shardCount === 0) {
    throw new Error('Discord Gateway has no active shards.');
  }

  const shardId = calculateShardId(guildId, shardCount);
  const shard = client.ws.shards.get(shardId);

  if (!shard) {
    throw new Error(`Discord Gateway shard ${shardId} is not connected.`);
  }

  shard.send(payload);
}

async function close(): Promise<void> {
  if (shutdown) return shutdown;

  shutdown = (runtime?.close() ?? Promise.resolve()).finally(() => {
    client.destroy();
  });

  return shutdown;
}

function handleShutdown(): void {
  void close().catch((error: unknown) => {
    console.error('Shutdown failed:', error);
  });
}

process.once('SIGINT', handleShutdown);
process.once('SIGTERM', handleShutdown);

try {
  await client.login(token);
  const readyClient = await ready;

  const remedia = await RemediaClient.create({
    botUserId: readyClient.user.id,
    nodes: [
      {
        type: 'remote',
        id: 'remote',
        url: 'remedia://remedia:password@127.0.0.1:8787',
      },
      { type: 'embedded', id: 'embedded' },
    ],
    send: sendGatewayPayload,
  });

  runtime = VoiceRuntime.start(remedia);
} catch (error) {
  await close().catch((closeError: unknown) => {
    console.error('Cleanup after Gateway login failure also failed:', closeError);
  });

  throw error;
}
