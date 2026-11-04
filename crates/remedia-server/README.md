# remedia-server

`remedia-server` is Remedia's standalone Rust server. It exposes authenticated, versioned JSON RPC
over WebSocket and owns server-side client sessions, guild players, native playback, and optional
Discord voice transports. It uses `remedia-core`, `remedia-discord`, and
`remedia-observability` directly; it does not use N-API or an FFmpeg subprocess.

The server starts only with `remedia.config.json` in the current working directory, or with an
explicit `--config <path>` argument. See [`remedia.config.example.json`](remedia.config.example.json)
for the complete schema. If the file is missing or a configured media root cannot be resolved, the
startup error reports the path. Relative media roots are based on the config file's directory, and
`~`/`~/...` roots expand using the current user's home directory.

```sh
cp crates/remedia-server/remedia.config.example.json remedia.config.json
mkdir -p media
export REMEDIA_RPC_PASSWORD='replace-this'
export REMEDIA_BOT_USER_ID='100000000000000001'
cargo run -p remedia-server
```

The default bind address is `127.0.0.1:8787`. Keep the server on loopback or a private network
unless a TLS reverse proxy protects WebSocket traffic. Basic authentication credentials and voice
credentials must never cross an unencrypted public connection.

Optional `audio.opus` settings control PCM encoding for all players: `bitrate` defaults to 128000
**bits per second**, `complexity` to 9, and `vbr` to true. Each player output owns an independent,
lazy encoder. Compatible Opus passthrough forwards original packets and is unaffected. See the
[configuration guide](../../apps/docs/content/docs/server/configuration.mdx#opus-encoder-settings)
for ranges, quality and CPU tradeoffs, and partial configuration examples.

## Connection and ownership

Connect a WebSocket client to `ws://127.0.0.1:8787/v1/ws` and authenticate the handshake with
`Authorization: Basic ...` and `X-Remedia-Bot-Id: <discord-snowflake>`. The manual integration
example reads the ID from `REMEDIA_BOT_USER_ID`. The bot ID is sent on each WebSocket connection,
validated as a positive decimal snowflake, and bound to that connection's
client session. It is not part of server configuration, so multiple bot identities can use the same
server account and server instance. Each client session owns guild sessions; each guild session
owns one `PlaybackSession` and an optional voice transport. Guild IDs are scoped by client session,
so separate bots can play in the same guild independently.

The bot application still owns its Discord Gateway connection. It supplies `voice.connect` with
fresh gateway dispatch credentials; the server handles the voice transport, RTP, DAVE, and UDP. The
server does not send Gateway OP 4 or join channels.

If a WebSocket disappears unexpectedly, its session remains active for the configured resume
timeout. The same authenticated user can reclaim it with its opaque resume credential. Event
delivery is bounded and events produced while detached are not replayed; `client.resume` returns a
snapshot. Explicit `client.close` and expiry stop playback, disconnect transports, and release
resources. Sessions do not survive a server process restart.

## RPC commands

| Command                                                                         | Purpose                                                                        |
| ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| `node.info`, `node.protocol_version`, `node.health`, `node.stats`               | Node identity, protocol, health, and capacity/host metrics                     |
| `client.create`, `client.list`, `client.resume`, `client.close`                 | Create, inspect, reclaim, and close an authenticated client session            |
| `guild.create`, `guild.list`, `guild.get`, `guild.destroy`                      | Manage players scoped to the controlled client session                         |
| `voice.connect`, `voice.state`, `voice.disconnect`                              | Connect supplied Discord voice credentials or inspect/disconnect the transport |
| `player.play`, `player.pause`, `player.resume`, `player.stop`, `player.seek`    | Play an allowed file or HTTP(S) URL and control it                             |
| `player.filter.set`, `player.filter.clear`, `player.volume.set`, `player.state` | Set a raw FFmpeg filtergraph, linear gain, and inspect playback                |

`player.play` accepts `{"type":"url","url":"https://..."}` or
`{"type":"file","path":"/approved/path/file.ogg"}`. File inputs must resolve inside a configured
`allowedFileRoots` directory. URLs require a configured `allowedUrlHosts` entry; private and special
IP ranges are rejected by default. FFmpeg may follow remote redirects, so deployments that accept
untrusted URLs should also enforce outbound network policy at the host or firewall.

With no voice transport, `player.play` decodes to a null output for validation and local server
workflows; it is not audible. A connected voice transport sends through the native Opus and Discord
pipeline. A raw graph is trusted application configuration and should not be exposed directly to
untrusted clients. Master gain is linear from `0.0` through `2.0` and is independent from filters.

RPC requests use `{ "version": 1, "id": "...", "command": "...", "params": {} }`. Responses keep
the same `id` and contain `ok` plus either `result` or `{ "code", "message" }`. Lifecycle events are
separate frames and may be lost when a bounded queue overflows; `client.events_lost` reports the
number dropped. Error codes include `invalid_request`, `authentication_failed`, `unknown_session`,
`invalid_state`, `resource_exhausted`, `request_timeout`, `unsupported`, `playback_failed`, and
`transport_failed`.

See the [Fumadocs server guide](../../apps/docs/content/docs/server/overview.mdx),
[configuration reference](../../apps/docs/content/docs/server/configuration.mdx), and
[protocol reference](../../apps/docs/content/docs/server/protocol.mdx) for the full schema and wire
examples.

## Manual Discord example

After the bot application has requested the channel join and collected a fresh
`VOICE_STATE_UPDATE`/`VOICE_SERVER_UPDATE` pair, the example can exercise playback, live filter
replacement, and cleanup without printing credentials:

```sh
export REMEDIA_SERVER_URL='ws://127.0.0.1:8787/v1/ws'
export REMEDIA_USERNAME='media-bot'
export REMEDIA_PASSWORD='replace-this'
export REMEDIA_BOT_USER_ID='100000000000000001'
export REMEDIA_GUILD_ID='200000000000000001'
export REMEDIA_CHANNEL_ID='200000000000000002'
export REMEDIA_MEDIA_URL='https://media.example.com/audio.ogg'
export REMEDIA_VOICE_SESSION_ID='value-from-voice-state-update'
export REMEDIA_VOICE_TOKEN='value-from-voice-server-update'
export REMEDIA_VOICE_ENDPOINT='endpoint-from-voice-server-update'
cargo run -p remedia-server --example remote_discord
```

The example does not implement Discord Gateway or OP 4. It is a manual integration harness; CI uses
localhost WebSocket tests and a null output rather than Discord credentials.

## Multi-node operation

Every process is independent and reports a node-local `nodeId`, version, active player count,
capacity, CPU parallelism/load when available, process RSS when available, and transport/processing
health. `@remedia/client` uses this information to select a node when creating a guild session and
routes later commands back to that node. The server never migrates a player.
