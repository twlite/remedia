# @remedia/client

`@remedia/client` is the high-level TypeScript orchestration SDK for embedded and remote Remedia
nodes. It routes Discord sessions directly to an embedded `@remedia/node` addon or to authenticated
`remedia-server` WebSocket RPC. Remote-only applications do not need the native addon; install
`@remedia/node` as an optional peer only when configuring an embedded node.

```ts
import { RemediaClient } from '@remedia/client';

const remedia = await RemediaClient.create({
  nodes: [
    { type: 'embedded', id: 'local' },
    {
      type: 'remote',
      id: 'media-a',
      url: 'remedias://media.example.net',
      auth: { username: 'bot', password: process.env.REMEDIA_PASSWORD! },
    },
  ],
  send(guildId, payload) {
    sendToShard(guildId, payload);
  },
});

discord.on('raw', (packet) => remedia.receive(packet));
const voice = remedia.createDiscordSession({ guildId, channelId, userId });
await voice.connect();
await voice.play({ url: 'https://media.example.net/audio.mp3' });
```

Guilds are assigned to healthy nodes using player capacity and remain sticky to that node. Remote
sessions reconnect and attempt server-side session resumption. The SDK reports expired sessions;
it does not migrate active playback or Discord voice transports across nodes. Remote nodes accept
server-accessible URLs and file paths. Buffer and Readable inputs require an embedded node selected
with `nodeId`.

Disposing a guild while its remote node is temporarily unavailable defers remote cleanup until the
same node session resumes. If the client closes while that node is unreachable, server resources
expire with the configured session-resumption timeout.

See the [Fumadocs client guide](../../apps/docs/content/docs/client/overview.mdx) for connection
strings, configuration, lifecycle, and recovery semantics.

## Development

```sh
pnpm --filter @remedia/client build
pnpm --filter @remedia/client typecheck
pnpm --filter @remedia/client test
```

The hybrid-node test additionally builds the native addon and requires FFmpeg and libopus development
libraries; see the repository contributor guide.
