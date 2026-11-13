# Remedia

Remedia is an audio engine for Discord bots. Give it a URL, a file, or audio from your own plugin;
it decodes the media, applies filters and volume, and plays it into a voice channel over Discord's
encrypted voice transport. Run it inside your Node.js bot, or on a separate server.

Your application keeps its own Discord Gateway connection and decides what to play. Remedia is not a
music search service or a Lavalink-compatible server.

For a batteries included framework for Remedia, use [Discord Player](https://discord-player.js.org).
It provides a queue system and other bot features so you do not have to set them up manually.

## Get started

Pick how you want to run the engine:

| I want to...                                 | Install                                                                                                          | Guide                                                                           |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Run audio inside my Node.js bot              | `npm install @remedia/node` and [FFmpeg 9](https://remedia.twlite.dev/docs/node/ffmpeg-9)                        | [Embedded quickstart](https://remedia.twlite.dev/docs/getting-started/embedded) |
| Run audio on a separate server               | `curl -fsSL https://remedia.twlite.dev/install.sh \| sh` on the server, `npm install @remedia/client` in the bot | [Remote quickstart](https://remedia.twlite.dev/docs/getting-started/remote)     |
| Use both from one API                        | `npm install @remedia/client`                                                                                    | [Using `@remedia/client`](https://remedia.twlite.dev/docs/client/overview)      |
| Resolve audio or data through my own service | `npm install @remedia/plugin-host`                                                                               | [Plugins](https://remedia.twlite.dev/docs/plugins/overview)                     |

To run the server in a container instead, see the [Docker guide](https://remedia.twlite.dev/docs/server/docker) and [`docker/`](docker).

Node.js 22.18 or newer is required. FFmpeg 9 shared libraries are needed on the machine that decodes
audio; a bot that only talks to remote servers needs none. On Windows, use the PowerShell installer
from the [server installation guide](https://remedia.twlite.dev/docs/server/installation).

## Example

```ts
import { Remedia } from '@remedia/node';

const remedia = await Remedia.create({
  send: (guildId, payload) => discord.guilds.cache.get(guildId)?.shard.send(payload),
});
discord.on('raw', (packet) => remedia.receive(packet));

const voice = remedia.createDiscordSession({ guildId, channelId, userId: discord.user.id });

await voice.connect();
await voice.play({ url: 'https://example.com/audio.mp3' });
await voice.setVolume(0.8);
```

## Packages

| Package                                                | Purpose                                                            |
| ------------------------------------------------------ | ------------------------------------------------------------------ |
| [`@remedia/node`](packages/remedia-node)               | Embedded engine for Node.js                                        |
| [`@remedia/client`](packages/remedia-client)           | One client for embedded and remote nodes                           |
| [`remedia-server`](crates/remedia-server)              | Standalone audio server and plugin-host management CLI             |
| [`@remedia/plugin-host`](packages/remedia-plugin-host) | Build plugin services for audio and application-defined operations |
| [`@remedia/config`](packages/remedia-config)           | Validation and JSON Schema for the server configuration            |

The engine is written in Rust ([`remedia-core`](crates/remedia-core),
[`remedia-discord`](crates/remedia-discord)); the Rust APIs are documented for contributors and
embedders.

## Documentation

Read the documentation at **[remedia.twlite.dev/docs](https://remedia.twlite.dev/docs)**. Its source
is in [`apps/docs`](apps/docs).

## Community

Join the [AndrozDev Discord server](https://androz2091.fr/discord) to get help and share what you build.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for toolchains, native dependencies, tests, and release
workflows. The workspace uses Rust and pnpm.

## License

[MIT](LICENSE). See [third-party notices](THIRD_PARTY_NOTICES.md) for dependency terms.
Applications are responsible for obtaining authorized media sources.
