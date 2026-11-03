# Discord integration harness

This app demonstrates `@remedia/client` using discord.js. It provides slash
commands for HTTP(S) playback, an in-memory per-guild queue, pause, resume,
seek, skip, stop, now playing, disconnect, six named `/filter` presets, and
`/volume`. The app owns the queue and preset mapping; Remedia owns decoding and
voice transport. Each guild queue holds
up to 100 upcoming tracks.
Playback replies prefer the title returned in Remedia's media metadata and
fall back to a URL with credentials and query parameters removed. Queue entries
show URLs until Remedia opens them and returns metadata.
Slash command responses use consistent Remedia embeds, with colored error
embeds when a command fails. Successful responses are public and identify the
invoking user; errors remain ephemeral. `/nowplaying` shows elapsed time, a
proportional progress indicator, and original media duration, marking estimated
source positions with `~`. Unknown or live duration does not show a fabricated
progress bar. The bot subscribes to typed Remedia lifecycle events and posts
autonomous notifications for queued track starts, genuine playback failures,
and terminal voice failures. Command-confirmed controls, filters, volume, stop,
skip, and disconnect do not produce a second lifecycle embed. The text channel
from the command that
creates a guild's queue is retained in that queue's in-memory metadata and is
used for those notifications across queued tracks. Later `/play` requests do
not silently move the queue to another text channel. Disconnecting disposes
the queue metadata. Automatic start notifications identify the track requester
and are labeled automatic; `/queue` remains a public snapshot.

Create `.env` with `DISCORD_TOKEN` and `APPLICATION_ID`, then choose a backend.
Embedded mode is the default and calls the native Node addon directly. Build it
once and run the bot from the repository root:

```sh
pnpm install --frozen-lockfile
pnpm --filter @remedia/node build
pnpm --filter discord-app deploy
pnpm --filter discord-app start
```

For remote mode, set `REMEDIA_MODE=remote` and provide `REMEDIA_NODES` as a
JSON array. Each entry contains a node `id`, `url`, and Basic auth credentials:

```dotenv
REMEDIA_MODE=remote
REMEDIA_NODES='[
  {"id":"node-a","url":"remedia://127.0.0.1:8787","auth":{"username":"bot","password":"secret"}},
  {"id":"node-b","url":"remedias://media.example.com","auth":{"username":"bot","password":"secret"}}
]'
```

Then deploy and start as usual; remote mode does not build or load the native
addon. The bot uses the same queue and slash command logic for both backends.
After Discord Gateway READY, the bot sends its own user ID in the authenticated Remedia
WebSocket handshake. No Discord bot ID belongs in the server config or node credentials.
New guilds are assigned to healthy nodes using player capacity, and each guild
stays on its original node. Temporary WebSocket loss attempts session
resumption. An expired server session is reported and is not silently migrated
to another node. HTTP(S) URLs and paths accessible to the server work remotely;
Buffer and Readable sources require an embedded node.

`start` runs TypeScript source with `tsx`; it does not compile the bot to
`dist/`. `typecheck` still runs `tsc --noEmit`. The bot needs the `Guilds` and
`GuildVoiceStates` Gateway intents. It accepts direct HTTP(S) URLs supported by
the installed FFmpeg libraries and does not perform URL extraction or search.
See the [Discord harness guide](/docs/discord/bot-harness) for command behavior
and lifecycle details.

`/filter set` accepts `8d`, `bassboost`, `nightcore`, `vaporwave`, `tremolo`,
or `vibrato` and toggles that preset. A newly selected preset is appended to the
active filters; selecting it again removes only that preset, leaving the others
in order. If the last preset is toggled off, its graph is cleared. `/filter list`
shows active presets, and `/filter clear` removes them all. These names are
harness conveniences mapped to native FFmpeg audio graphs, not presets in
`remedia-core` or either Node package. `/volume` reads the current value and
`/volume percent:<0..200>` applies independent linear gain;
gains above 100% may clip. Filter and volume values stay scoped to one guild
and carry across queued tracks and skips until the player disconnects. The
queue manager serializes skips and ignores completion notifications from a
replaced playback generation.

`/loop mode:<off|track|queue>` selects a per-guild loop mode. Track mode repeats
the current item after natural completion, while `/skip` still advances. Queue
mode rotates naturally completed and manually skipped tracks to the end of the
queue; failed tracks are not repeated. The mode remains selected across tracks
and `/stop`, and resets when the player disconnects. `/nowplaying` displays the
active mode.
