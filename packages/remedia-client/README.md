# @remedia/client

`@remedia/client` is the high-level TypeScript orchestration SDK for embedded and remote Remedia
nodes. It routes Discord sessions directly to an embedded `@remedia/node` addon or to authenticated
`remedia-server` WebSocket RPC. Remote-only applications do not need the native addon; install
`@remedia/node` as an optional peer only when configuring an embedded node.

## Installation

Requires Node.js **22.18.0 or newer** and ESM.

```sh
# Remote only: FFmpeg runs on the server, not the client machine
npm install @remedia/client
# Add this only for embedded nodes
npm install @remedia/node
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
it supports explicit cross-node migration for connected sessions, with replay-and-seek recovery for active HTTP(S) URL playback. Remote nodes accept
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

## Plugin operations

Remote nodes forward generic, operator-allowlisted JSON operations to their configured plugin hosts,
so your application only configures Remedia nodes:

```ts
const hosts = await client.plugins.list({ nodeId: 'remote' });
const outcome = await client.plugins.execute({
  nodeId: 'remote',
  hostId: 'source-host-1',
  pluginId: 'example.metadata',
  operation: 'catalog.search',
  input: { query: 'some query' },
  signal: controller.signal,
});

if (outcome.handled) console.log(outcome.result);
```

Results are `{ handled: true, result }` (any JSON value) or `{ handled: false }`. Operation semantics
belong to your application. Embedded nodes and older servers report
`PluginOperationsUnsupportedError`. See the documentation for node selection and cancellation.

## Cross-node migration

`await voice.migrate('media-b')` moves an already connected guild between configured healthy
nodes. The client reserves the destination first, then closes the old transport and decoder,
updates the sticky assignment, reconnects using fresh Gateway credentials, and restores its
volume, filtergraph, channel, and self flags. The original `DiscordSession` object stays usable.
Observe `RemediaClientEventNames.guildMigrated` for a successful assignment cutover.

When playing or paused, only a replayable `{ url }` source with a known media position can
migrate. Remedia starts a **new playback generation** on the destination and seeks to the
snapshot's source position, restoring pause state and application `customMetadata`. This is not
gapless playback and is not a decoder/packet transfer. Buffer, Readable, local file, and
unknown-position active sources are rejected before cutover. A failed destination connection
after cutover leaves that node assigned; the application can retry `connect()` and playback.
Migration is explicit, not automatic load shedding or failover.

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

## Session configuration and channel movement

A client owns one session per guild. Separate clients (including separate bot users) can use the
same guild and channel with independent playback, transport generations, and events. `userId` is
the Discord bot user ID, not a Remedia client session ID. Channel IDs are mutable destinations.
Duplicate guild registration on the same client throws; dispose the old session before replacing it.

```ts
const session = remedia.createDiscordSession({
  guildId,
  channelId,
  userId,
  selfDeaf: true,
  selfMute: false,
  volume: 0.75,
  filterGraph: 'bass=gain=3',
});
await session.connect();
await session.move(otherChannelId);
await session.setSelfDeaf(false);
await session.setSelfMute(true);
await session.dispose();
```

`selfDeaf` defaults to `true` (previously joins always sent `false`); `selfMute` defaults to
`false`. These flags affect Discord voice state, independently of playback pause and gain.
Setters retain values before connection and update Gateway OP 4 after joining. Unchanged values
send nothing. Joins, credential refreshes, and moves preserve the flags. Keep the library-agnostic
`send(guildId, payload)` and `receive(payload)` Gateway integration wired throughout the session.

Initial `volume` is finite linear gain from 0 through 2, default 1. `filterGraph` is a trusted raw
FFmpeg audio filtergraph or `null` (default). Invalid gain and empty/non-string graphs are rejected
at creation; native FFmpeg validates graph syntax when the backend is allocated/configured, before
playback is allowed. Configuration is serialized with runtime setters and applied before audio
starts. Both values persist across tracks and transport replacement; existing `setVolume()` and
`filters.set()`/`clear()` remain available with their existing change events.

`move(channelId): Promise<void>` requests fresh Gateway credentials and replaces the existing voice
transport without restarting or seeking playback. Decoder input (including one-shot Readables),
filters, gain, playback generation, and paused state are retained. A move to the current destination
shares a pending connection or resolves immediately when connected. A newer destination supersedes
a pending attempt with an `AbortError`; `connect()` shares the latest attempt. Moving while explicit
disconnection or disposal is underway rejects. `disconnect()`/`dispose()` cancel a pending move.
`channelId` reports the latest requested destination, including after a failed move.

Intentional moves do not use the automatic recovery retry policy. A failed move reports a failed
voice state and leaves playback parked with bounded buffering; call `connect()` to retry the
requested destination, `move()` to choose another, or `disconnect()`/`dispose()` to release it.
Ordinary transport recovery retains its existing bounded credential-refresh retries. Transport
replacement itself does not emit playback stopped/completed/failed events.

Gateway mocks verify orchestration and local native tests verify media lifecycles. Real Discord
channel movement, same-channel multi-bot voice, credential ordering, and self flags still require
an application integration test with Discord. Forward only each bot's own Gateway dispatch stream.
