# @remedia/client

`@remedia/client` is the high-level TypeScript orchestration SDK for embedded and remote Remedia
nodes. It routes Discord sessions directly to an embedded `@remedia/node` addon or to authenticated
`remedia-server` WebSocket RPC. Remote-only applications do not need the native addon; install
`@remedia/node` as an optional peer only when configuring an embedded node.

## Installation

Requires Node.js **22.18.0 or newer** and ESM.

```sh
# Remote only: FFmpeg runs on the server, not the client machine
npm install @remedia/client@0.1.0-beta.1
# Add this only for embedded nodes
npm install @remedia/node@0.1.0-beta.1
```

The following remote-only example assumes an authenticated Discord Gateway client named `discord`
and a `sendToShard` function that forwards payloads to its owning shard. Run after READY, and
supply your guild/channel/user IDs. Call `await remedia.close()` during application shutdown.
For embedded usage add `{ type: 'embedded', id: 'local' }` to `nodes`; the native package requires
independently installed FFmpeg 7 on that machine. See the
[distribution guide](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/node/distribution.mdx)
for supported architectures, ABI versions, and native load troubleshooting.

```ts
import { RemediaClient } from '@remedia/client';

const remedia = await RemediaClient.create({
  // Obtain this from the host Discord client's READY event.
  botUserId: discord.user.id,
  nodes: [
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

Remote nodes require `botUserId`. The SDK sends it as the `X-Remedia-Bot-Id` WebSocket handshake
header and uses it to bind the server session and voice transport to this bot. A server account can
therefore be reused by independent bot clients; each client must create its own `RemediaClient` with
its own bot ID.

Guilds are assigned to healthy nodes using player capacity and remain sticky to that node. Remote
sessions reconnect and attempt server-side session resumption. The SDK reports expired sessions;
it does not migrate active playback or Discord voice transports across nodes. Remote nodes accept
server-accessible URLs and file paths. Buffer and Readable inputs require an embedded node selected
with `nodeId`.

Disposing a guild while its remote node is temporarily unavailable defers remote cleanup until the
same node session resumes. If the client closes while that node is unreachable, server resources
expire with the configured session-resumption timeout.

`DiscordSession` and `RemediaClient` expose typed Node `EventEmitter` lifecycle events using the
exported `PlaybackSessionEvents`, `VoiceSessionEvents`, and `RemediaClientEventNames` constants.
Playback events carry a source generation and optional local `customMetadata`; awaited
`hooks.beforePlay()` can cancel a start before it reaches a node. Event completion reflects engine
EOF, not necessarily drained Discord buffers. See the
[hybrid client guide](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/client/overview.mdx) for event payloads,
reconnection reconciliation, and metadata lifetime.

See the [Fumadocs client guide](https://github.com/twlite/remedia/blob/main/apps/docs/content/docs/client/overview.mdx) for connection
strings, configuration, lifecycle, and recovery semantics.

## Development

```sh
pnpm --filter @remedia/client build
pnpm --filter @remedia/client typecheck
pnpm --filter @remedia/client test
```

The hybrid-node test additionally builds the native addon and requires FFmpeg development libraries;
libopus is built from bundled source and statically linked. See the repository contributor guide.

## License and media responsibility

Remedia is a general-purpose media processing and transport engine. It does not provide media
content or grant rights to access or use third-party media. Developers and operators are responsible
for ensuring their use complies with applicable laws, intellectual property rights, and third-party
service terms.

Remedia is provided under the [MIT License](LICENSE) without warranty. Dependency terms remain
separate; see [third-party notices](THIRD_PARTY_NOTICES.md), including bundled libopus and
independently installed FFmpeg.
