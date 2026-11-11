# @remedia/client

One TypeScript SDK for playing audio into Discord voice channels. It runs the Remedia engine inside
your process (an **embedded** node), on `remedia-server` processes (**remote** nodes), or both,
behind the same session API. Each guild stays on the node it was assigned to.

Requires Node.js 22.18 or newer (ESM).

## Install

```sh
npm install @remedia/client
```

A bot that only uses remote nodes needs nothing else: no FFmpeg and no native addon on the bot
machine. FFmpeg 9 is installed on the `remedia-server` host (see the
[server installation guide](https://remedia.twlite.dev/docs/server/installation)).

For an embedded node, also install `@remedia/node` and
[FFmpeg 9](https://remedia.twlite.dev/docs/node/ffmpeg-9):

```sh
npm install @remedia/node
```

## Example

```ts
import { RemediaClient } from '@remedia/client';

const remedia = await RemediaClient.create({
  botUserId: discord.user.id,
  nodes: [
    {
      type: 'remote',
      id: 'media-1',
      url: 'remedias://media.example.net',
      auth: { username: 'media-bot', password: process.env.REMEDIA_RPC_PASSWORD! },
    },
    // Optionally add an embedded node as well:
    // { type: 'embedded', id: 'local' },
  ],
  send: (guildId, payload) => discord.guilds.cache.get(guildId)?.shard.send(payload),
});
discord.on('raw', (packet) => remedia.receive(packet));

const voice = remedia.createDiscordSession({ guildId, channelId, userId: discord.user.id });

await voice.connect();
await voice.play({ url: 'https://media.example.net/audio.mp3' });
await voice.setVolume(0.8);

await remedia.close();
```

`botUserId` is required when any node is remote. Remote nodes open URLs and files themselves, so a
`{ file }` is a path on the server; buffers and Readables need an embedded node.

## Plugin operations

Remote nodes can forward application-defined operations to plugin hosts configured on the server,
so your bot never needs the host's address:

```ts
const result = await remedia.plugins.execute({
  nodeId: 'media-1',
  hostId: 'source-host-1',
  pluginId: 'example.metadata',
  operation: 'catalog.search', // an example operation defined by your plugin host
  input: { query: 'example' },
});

if (result.handled) console.log(result.result);
```

The operation must be allowlisted in the server configuration. Embedded nodes do not support
forwarding.

## Documentation

- [Remote quickstart](https://remedia.twlite.dev/docs/getting-started/remote) and [embedded quickstart](https://remedia.twlite.dev/docs/getting-started/embedded)
- [Using `@remedia/client`](https://remedia.twlite.dev/docs/client/overview)
- [Nodes and routing](https://remedia.twlite.dev/docs/client/nodes), [events and hooks](https://remedia.twlite.dev/docs/client/events), [session recovery](https://remedia.twlite.dev/docs/client/sessions)
- [Executing plugin operations](https://remedia.twlite.dev/docs/server/plugin-forwarding)
- [API reference](https://remedia.twlite.dev/docs/client/reference)

## License and media responsibility

Remedia is a general-purpose media processing and transport engine. It does not provide media
content or grant rights to access or use third-party media. You are responsible for ensuring your
use complies with applicable laws, intellectual property rights, and third-party service terms.

Remedia is provided under the [MIT License](./LICENSE) without warranty. See
[third-party notices](./THIRD_PARTY_NOTICES.md).
