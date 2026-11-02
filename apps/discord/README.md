# Discord integration harness

This app demonstrates `@remedia/node` using discord.js. It provides slash
commands for HTTP(S) playback, an in-memory per-guild queue, pause, resume,
seek, skip, stop, now playing, and disconnect. The app owns the queue; the
native Remedia API owns decoding and voice transport. Each guild queue holds
up to 100 upcoming tracks.

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
