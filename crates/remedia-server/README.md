# remedia-server

A standalone audio server for Remedia. Your Discord bot connects to it with
[`@remedia/client`](../../packages/remedia-client); the server decodes media and sends audio to Discord
voice channels, so the bot machine needs no FFmpeg. The package also provides the `remedia-server` /
`remedia` command line.

## Install

```sh
curl -fsSL https://remedia.twlite.dev/install.sh | sh
```

On Windows PowerShell: `irm https://remedia.twlite.dev/install.ps1 | iex`. The installer provisions
`remedia-server` together with FFmpeg 9. No Rust toolchain is required. See the
[installation guide](https://remedia.twlite.dev/docs/server/installation).

## Run it

```sh
remedia-server --generate-config-file   # writes a safe starter remedia.config.json
export REMEDIA_RPC_PASSWORD='a-long-random-password'
remedia-server doctor                   # checks FFmpeg libraries and the configuration
remedia-server                          # starts the server on 127.0.0.1:8787
```

Then connect from your bot:

```ts
import { RemediaClient } from '@remedia/client';

const remedia = await RemediaClient.create({
  botUserId: discord.user.id,
  nodes: [
    {
      type: 'remote',
      id: 'media-1',
      url: 'remedia://127.0.0.1:8787',
      auth: { username: 'media-bot', password: process.env.REMEDIA_RPC_PASSWORD! },
    },
  ],
  send: (guildId, payload) => discord.guilds.cache.get(guildId)?.shard.send(payload),
});
```

Keep the server on loopback or a private network unless a TLS reverse proxy protects it.

## Commands

| Command                                 | Purpose                                                                              |
| --------------------------------------- | ------------------------------------------------------------------------------------ |
| `remedia-server [serve]`                | Start the server (`--config`, `--env-file`, `--optional-env-file`)                   |
| `remedia-server doctor`                 | Check FFmpeg libraries, configuration, and plugin hosts                              |
| `remedia-server --generate-config-file` | Create a starter configuration                                                       |
| `remedia-server --list [--json]`        | Show running servers                                                                 |
| `remedia-server plugins ...`            | Register and manage [plugin hosts](https://remedia.twlite.dev/docs/plugins/overview) |
| `remedia-server update`                 | Install the latest release                                                           |

Run `remedia-server --help` for every option. Everything except `serve` and `doctor` works without
FFmpeg installed.

## Documentation

- [Server overview](https://remedia.twlite.dev/docs/server/overview) and [remote quickstart](https://remedia.twlite.dev/docs/getting-started/remote)
- [Configuration](https://remedia.twlite.dev/docs/server/configuration) and [CLI](https://remedia.twlite.dev/docs/server/cli)
- [Deployment](https://remedia.twlite.dev/docs/server/deployment)
- [WebSocket RPC protocol](https://remedia.twlite.dev/docs/server/protocol), for writing your own client

## Development

Contributors can build from source with `cargo run -p remedia-server`, which needs the FFmpeg 9
development libraries; see [CONTRIBUTING.md](../../CONTRIBUTING.md). [`remedia.config.example.json`](remedia.config.example.json)
shows every configuration key. The FFmpeg-free configuration and plugin-host code is in
[`remedia-server-config`](../remedia-server-config).
