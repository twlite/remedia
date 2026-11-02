# Discord integration harness

This app demonstrates `@remedia/node` using discord.js. It provides slash
commands for HTTP(S) playback, an in-memory per-guild queue, pause, resume,
seek, skip, stop, now playing, disconnect, six named `/filter` presets, and
`/volume`. The app owns the queue and preset mapping; the
native Remedia API owns decoding and voice transport. Each guild queue holds
up to 100 upcoming tracks.
Playback replies prefer the title returned in Remedia's media metadata and
fall back to a URL with credentials and query parameters removed. Queue entries
show URLs until Remedia opens them and returns metadata.

Create `.env` with `DISCORD_TOKEN` and `APPLICATION_ID`, then run from the
repository root:

```sh
pnpm install --frozen-lockfile
pnpm --filter @remedia/node build
pnpm --filter discord-app deploy
pnpm --filter discord-app start
```

`start` runs TypeScript source with `tsx`; it does not compile the bot to
`dist/`. `typecheck` still runs `tsc --noEmit`. The bot needs the `Guilds` and
`GuildVoiceStates` Gateway intents. It accepts direct HTTP(S) URLs supported by
the installed FFmpeg libraries and does not perform URL extraction or search.
See the [Discord harness guide](/docs/discord/bot-harness) for command behavior
and lifecycle details.

`/filter set` accepts `8d`, `bassboost`, `nightcore`, `vaporwave`, `tremolo`,
or `vibrato`; `/filter clear` and `/filter list` manage and display the current
selection. These names are harness conveniences mapped to native FFmpeg audio
graphs, not presets in `remedia-core` or `@remedia/node`. `/volume` reads the
current value and `/volume percent:<0..200>` applies independent linear gain;
gains above 100% may clip. Filter and volume values stay scoped to one guild
and carry across queued tracks and skips until the player disconnects. The
queue manager serializes skips and ignores completion notifications from a
replaced playback generation.
