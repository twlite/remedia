# remedia-discord

`remedia-discord` is a focused outbound Discord voice transport. It accepts already-encoded Opus
packets and owns the voice WebSocket, UDP discovery, RTP packetization, negotiated transport
encryption, DAVE session, packet scheduling, and connection lifecycle. It does not decode,
resample, or encode audio.

The caller must use its own Discord Gateway or bot library to join a voice channel and provide the
guild, channel, user, session, token, and endpoint information. This crate does not provide a bot
Gateway client, REST client, or command framework. `remedia-core` supplies the transport-independent
`OpusEncoderOutput` adapter for PCM playback.

The transport negotiates Voice Gateway v8 and requires DAVE media encryption. The selected `davey`
release supports DAVE protocol version 1; unsupported versions fail explicitly. This implementation
has not been validated against a live Discord bot, so live compatibility remains unverified.

`connect_with_observability` accepts an application-owned shared context for connection, DAVE, RTP,
and lifecycle diagnostics. `connect` remains available for applications that use their own global
tracing subscriber. Tokens, Discord voice session IDs, encryption keys, and media payloads are not
logged.

## Development

Run from the repository root:

```sh
cargo test -p remedia-discord
cargo clippy -p remedia-discord --all-targets -- -D warnings
cargo run -p remedia-discord --example play_file
```

The manual example reads connection credentials and a file or URL source from environment
variables. Keep credentials out of shell history and logs. See the
[Discord voice guide](../../apps/docs/content/docs/discord/voice-output.mdx) for the connection
flow, lifecycle, limitations, and full example. See the
[observability guide](../../apps/docs/content/docs/observability/configuration.mdx) for filters,
event routing, metrics, and provider lifecycle.
