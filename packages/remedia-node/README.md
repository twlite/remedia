# @remedia/node

Embed the Remedia audio engine in your Node.js process. It plays audio into Discord voice channels
and decodes local files, HTTP(S) URLs, in-memory bytes, and Node.js streams. The engine runs in the
same process; there is no server to start.

Requires Node.js 22.18 or newer (ESM) and FFmpeg 9 shared libraries on the same machine.

## Install

```sh
npm install @remedia/node
```

npm does not install FFmpeg. The Remedia installer provisions FFmpeg 9 only, into `~/.remedia/ffmpeg9`,
and `@remedia/node` finds it automatically:

```sh
curl -fsSL https://remedia.twlite.dev/install.sh | sh -s -- --ffmpeg-only
```

```powershell
& ([scriptblock]::Create((irm https://remedia.twlite.dev/install.ps1))) -FFmpegOnly
```

Other options (Homebrew, Scoop, Winget, `REMEDIA_FFMPEG_DIR`) are in
[Install FFmpeg 9](https://remedia.twlite.dev/docs/node/ffmpeg-9).

## Example

Your bot keeps its own Discord Gateway connection. Give Remedia a function that sends Gateway
payloads and forward raw packets to it:

```ts
import { Remedia } from '@remedia/node';

const remedia = await Remedia.create({
  send: (guildId, payload) => discord.guilds.cache.get(guildId)?.shard.send(payload),
});
discord.on('raw', (packet) => remedia.receive(packet));

const voice = remedia.createDiscordSession({
  guildId: '123456789012345678',
  channelId: '234567890123456789',
  userId: discord.user.id,
});

await voice.connect();
await voice.play({ url: 'https://example.com/audio.mp3' });
console.log(await voice.wait());

await voice.dispose();
await remedia.close();
```

`remedia.createPlaybackSession()` decodes without Discord into a null output. It is useful for tests
and inspection, and does not play through local speakers.

## Documentation

- [Embedded quickstart](https://remedia.twlite.dev/docs/getting-started/embedded)
- [`@remedia/node` reference](https://remedia.twlite.dev/docs/node/bindings): inputs, events, filters, diagnostics, probing
- [Native library troubleshooting](https://remedia.twlite.dev/docs/node/troubleshooting) and [platform support](https://remedia.twlite.dev/docs/node/distribution)
- Want embedded and remote nodes behind one API? Use [`@remedia/client`](https://remedia.twlite.dev/docs/client/overview)

## Development

Contributors: see [CONTRIBUTING.md](../../CONTRIBUTING.md). From the repository root,
`pnpm --filter @remedia/node test` builds a debug addon and runs the Node.js tests, and
`pnpm --filter @remedia/node build` creates the release addon. Rust code lives in `src/`,
TypeScript in `lib/`; `dist/` and the N-API loader files are generated.

## License and media responsibility

Remedia is a general-purpose media processing and transport engine. It does not provide media
content or grant rights to access or use third-party media. You are responsible for ensuring your
use complies with applicable laws, intellectual property rights, and third-party service terms.

Remedia is provided under the [MIT License](./LICENSE) without warranty. Dependency terms remain
separate; see [third-party notices](./THIRD_PARTY_NOTICES.md), including bundled libopus and
independently installed FFmpeg.
