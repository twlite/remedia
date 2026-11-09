# Architecture

This document distinguishes implemented behavior from design intentions. `remedia-core` implements
file, HTTP, buffer, and byte-stream decoding to PCM, playback sessions, generic output, and
transport-independent Opus encoding. `remedia-discord` implements one outbound voice connection
with DAVE and RTP transport encryption. `remedia-server` now provides authenticated, in-memory
WebSocket RPC over the native engine. The N-API package exposes native playback, Discord session,
and diagnostics APIs. `@remedia/client` orchestrates direct embedded and remote server nodes.

## Rust crates

- **`remedia-core`** opens local filesystem paths, HTTP(S) URLs, owned encoded bytes, and bounded
  byte streams with libavformat and selects an audio stream. Its demux packet reader is separate
  from a lazily created libavcodec decoder and libswresample converter, while the public
  `AudioDecoder` continues to return normalized PCM. For an encoded-Opus-capable output, playback
  can forward a conservative subset of untrimmed 48 kHz mono/stereo family-zero Opus packets
  directly; other sources, outputs, and processing configurations use PCM and, when applicable,
  the bundled libopus encoder. It uses native FFmpeg libraries rather than starting FFmpeg
  subprocesses.
  Playback sessions manage one source, lifecycle state, seeking, optional native libavfilter
  graphs, linear master gain, and frame delivery to a generic output. With no graph configured,
  PCM bypasses libavfilter. The generic Opus output adapter uses its statically linked libopus
  encoder lazily, accumulates
  variable PCM blocks into 20 ms stereo Opus packets, and marks a padded final packet's actual media
  duration. It shares one encoded packet sink between source passthrough and PCM encoding. Dynamic
  changes retain the demuxer and transport; up to 120 ms / 512 KiB of compressed history warms a
  decoder, and partial PCM is closed at a legal packet boundary before returning to passthrough.
  Packets requiring pre-skip, discard padding, or other trimming fall back to PCM. Filter-graph
  delayed output is discarded at live processing transitions, so a small gap or artifact can occur.
  Source media, processed output samples, and RTP time remain distinct. The standalone benchmark
  example compares passthrough, re-encoding, and filtered modes. The adapter does not pace packets
  or expose Discord types. Device output is not implemented.
- **`remedia-discord`** accepts already-encoded Opus packets and owns voice gateway v8, UDP
  discovery, DAVE MLS/media encryption through `davey`, RTP packetization, transport encryption,
  speaking state, real-time packet pacing, and connection cleanup. It does not decode or encode
  audio. The caller obtains voice session credentials through its own Discord Gateway client.
  `davey` currently supports DAVE protocol version 1; other negotiated versions fail explicitly.
  The connection API additionally requires the voice channel ID because DAVE binds its MLS group to
  that channel. No live Discord bot connection has been verified in CI.
- **`remedia-observability`** provides validated environment configuration, local tracing
  dispatchers, bounded structured diagnostic routing, and optional OTLP trace and metric exporters.
  Core and Discord emit `tracing` spans and events but never initialize a subscriber or global
  OpenTelemetry provider themselves. Applications can pass an `ObservabilityHandle` to playback
  sessions and voice connections, or explicitly request a process-global subscriber when they own
  that lifecycle. Export is off by default; the crate does not export OTLP logs.
- **`remedia-server`** is a standalone Rust process using the core, Discord, and observability crates
  directly. It authenticates HTTP Basic credentials and validates the positive Discord bot ID from
  `X-Remedia-Bot-Id` during the `/v1/ws` WebSocket handshake. The bot ID is per connection rather
  than static server configuration, allowing distinct bots to use the same server account. It
  implements strict version 1 JSON RPC for node stats, client-session resume, guild players, voice
  connection lifecycle, media playback, filters, volume, and state. Each guild has a stable bounded
  packet router independent of its replaceable voice transport, so a Gateway credential refresh
  does not recreate server playback. Explicit disconnect and guild destruction stop the source.
  Remote voice lifecycle RPCs carry a monotonic transport generation; stale connect, suspend, or
  failure operations are rejected even though request handlers execute concurrently.
  Configured `pluginHosts` are generic plugin-host deployments addressed only by operator-assigned
  IDs. The server discovers them (`/.well-known/remedia`), executes allow-listed operations with
  bearer authentication under per-host pending-operation limits, and plays the returned media grant
  through its own pinned, redirect-free HTTP client and a bounded forward-only byte stream, so
  FFmpeg never issues a `Range` request to a single-use grant. Operation preparation shares the
  guild's cancellation and generation model, including staged playback. The server does not
  use N-API, perform Discord Gateway OP 4, or implement Lavalink. Each
  authenticated client session owns guild sessions; each guild session owns one playback session
  and an optional Discord transport. Session state is
  node-local and in memory; disconnects can be resumed for a configured bounded interval, while
  process restarts do not preserve playback. Event buffering and all resource limits are bounded.
- **`remedia-node`** contains the Rust N-API crate and thin TypeScript wrapper. `Remedia` owns
  isolated native playback sessions, Discord voice connections, and one bounded diagnostics
  forwarder. Its TypeScript `DiscordGatewayAdapter` accepts the host application's Gateway send
  function and routes raw `VOICE_STATE_UPDATE`/`VOICE_SERVER_UPDATE` dispatches to one session per
  guild. A join sends OP 4 and waits for both fresh dispatches before invoking the existing native
  credential-based transport API; credentials are not part of the public Node API. A voice server
  or session change requests fresh Gateway credentials and replaces only the voice transport.
  Playback retains its decoder, source, output, pause state, filters, volume, source generation,
  and application metadata through a stable bounded sender router. During handoff, the per-session
  worker parks at the output boundary; packets accepted by the old transport but not delivered may
  be lost and are never resent with stale keys. Ordinary voice WebSocket Resume is handled natively
  and does not send a bot Gateway request. Native playback methods run on NAPI-RS's managed Tokio runtime; blocking core methods
  are dispatched to blocking workers. A caller-owned cancellation token covers the gap before a
  scheduled decoder worker starts, so concurrent stop, dispose, or close cannot leave a late
  playback worker behind. Completion waiters capture a source generation synchronously, so a delayed
  waiter cannot attach to a replacement track. Standalone sessions currently decode into
  `NullOutput`, while Discord sessions use the core Opus output adapter. Both session types expose
  raw filtergraphs and independent linear master volume. The addon accepts files, HTTP(S) URLs,
  owned Buffer/Uint8Array data, and binary Node.js Readables. Readable chunks are copied into a
  bounded native byte queue with backpressure; stopping or closing detaches and pauses a caller-owned
  Readable without destroying it. The addon does not install a global tracing subscriber. It is
  private and under development and does not implement server RPC.

TypeScript wrappers use Node's typed `EventEmitter<T>` contract. Exported
`PlaybackSessionEvents`, `VoiceSessionEvents`, and `RemediaEvents` `as const` objects provide the
canonical event names; matching string-union types and payload maps keep `on`, `once`, and `off`
type-safe. Lifecycle events are emitted after successful observed transitions and carry source
generations. Started means processing began, and completed means natural EOF was processed, not
that transport buffers drained. Application `customMetadata` stays in JavaScript, is associated
with one generation, and is released at terminal state/disposal. Awaited `beforePlay` hooks run
before native start and may cancel it. The high-level wrappers isolate synchronous listener throws
from engine/runtime work and surface a process warning.

The core decoder, playback, Opus, outbound Discord transport, server RPC, and N-API playback
integration exist. The Node package adapts an externally owned Discord Gateway but does not
implement Gateway connectivity, REST, or channel commands. Receiving audio and device output remain
design intentions. Add crates or packages only when implementation needs them.

`@remedia/client` is the high-level TypeScript orchestration layer. Its remote connections send the
host Discord client's bot ID in `X-Remedia-Bot-Id`; server sessions are bound to both the authenticated
principal and that ID. Its embedded adapter lazily loads
the optional `@remedia/node` peer and calls the N-API engine directly without a local server. Its
remote adapter uses authenticated WebSocket RPC, maintains one resumable server session per node,
reconciles guild/player snapshots, bounds pending requests, and reconnects with bounded exponential
backoff. New guilds are
selected across healthy nodes using reported player capacity and local assignments; each guild stays
on its assigned node. Events and snapshots expose node identity, health and assignment state. Remote
Buffer and Readable inputs are not uploaded; those sources require an embedded assignment. A client
or server process restart cannot automatically restore an in-memory session, and expired assignments
are reported lost rather than migrated. Cross-node recovery requires application-managed fresh
Discord voice credentials and is not implemented. The high-level client exposes the same typed
playback/voice lifecycle event contract plus node/session health events. Remote event gaps are
reconciled against authoritative snapshots; if a terminal transition is learned from a snapshot,
one terminal event is emitted with `reconstructed: true`, without inventing historical starts.
Application metadata is never sent over RPC or stored by the server. Neither package emits track,
queue, or extractor events; those remain application policy.

`apps/discord` is a manual discord.js integration harness over `@remedia/client`. It forwards raw
Gateway dispatches, sends OP 4 through the owning shard, and provides app-level slash commands with
an in-memory per-guild URL queue. Queueing stays out of the core and Node playback APIs. The bot
runs its TypeScript source with `tsx`; `tsc --noEmit` remains its type check. Its six named audio
presets, `/filter`, and `/volume` commands are harness-only conveniences; the raw graph API remains
generic. Successful command replies are public and attributed to the invoking user; errors remain
ephemeral. State changes already confirmed by a command do not generate duplicate posts, while
automatic track starts and genuine failures retain lifecycle notifications. `/nowplaying` shows
source-time progress with an estimate marker when needed. The application-owned queue stores
per-guild effect settings and preserves them across skips.

The bounded `DiagnosticReceiver` in `remedia-observability` is connected to the Node wrapper by one
dedicated forwarding worker and a bounded N-API `ThreadsafeFunction`. The receiver is per
`Remedia` context; clones compete for events and are not broadcasts. The wrapper exposes structured
events through `on('debug', listener)` and reports events dropped by either bounded queue. Embedded
contexts disable console output by default to avoid duplicating events handled by JavaScript. The
standalone server initializes the same shared context for its RPC and cleanup tasks. Its console
output can be disabled in server configuration while configured OTLP export remains available.

## Embedded and remote integration

Embedded Node.js integration provides local applications direct native access. Buffer and
Uint8Array inputs are copied into native-owned memory. A Node Readable is pumped incrementally into
a 512 KiB bounded queue, so decoder backpressure reaches the producer without creating an
unbounded audio buffer. Cancelling playback releases queued bytes and detaches the feeder; the
caller retains ownership of the Readable, which is paused but not destroyed.

Remote integration runs in a separate server process. The implemented v0 server accepts HTTP(S)
URLs permitted by its explicit host policy and local files canonicalized beneath configured
`allowedFileRoots`. Uploading arbitrary client-generated binary streams to the remote server is
intentionally deferred; the local N-API byte input does not add remote transport. The server
requires TLS termination at a trusted reverse proxy for remote deployment and uses independent
node-local session registries. `@remedia/client` performs capacity-aware selection and sticky guild
routing across independent nodes; it does not move existing playback across nodes.

## Processing and inputs

The implemented decoder accepts explicit file-path, HTTP(S)-URL, in-memory byte, and bounded
producer-backed byte stream variants. Custom byte sources use FFmpeg AVIO callbacks; memory inputs
support byte seeking, while producer streams are one-shot and non-seekable. The decoder probes the
container, chooses an audio stream, and yields frames incrementally as fast as the caller consumes
them. FFmpeg handles redirects and unknown HTTP content lengths. A configurable network I/O timeout
and cancellation signals can interrupt blocked operations. The decoder itself never paces audio.

FFmpeg resamples to normalized interleaved 48 kHz stereo `f32` samples in FL/FR order. Mono input is
duplicated to stereo; multichannel input is downmixed using its channel layout. The frame position is
tracked as a sample offset on the output timeline. Duration and stream metadata can be unknown. The
decoder supports timestamp-based seeking where the FFmpeg demuxer can seek; HTTP range behavior may
limit remote seeking.

Each `PlaybackSession` owns one blocking worker, one demux reader, an optional lazily initialized
decoder/resampler, and one output instance. It applies direct backpressure by synchronously handing
one owned PCM frame or encoded Opus packet to the output; it has no session-level audio queue. Pause
waits for an in-flight read or output call to reach a safe boundary. Stop cancels source/output work
and joins the worker. `NullOutput` records PCM consumption without real-time pacing. The session
distinguishes source position, submitted output position, and optional output-reported played
position.

An optional single-input/single-output `PcmFilterGraph` uses libavfilter after normalized PCM
decoding and before independent master gain. Replacements are built and validated before activation,
then installed at a worker frame boundary. Delayed output held inside the old graph is discarded;
PCM already accepted by the output is retained. Replacement does not perform a seek flush, reset
the Opus encoder, or advance Discord's sender generation. The Opus adapter keeps partial PCM and
validates exact integer sample positions, so packets remain contiguous across the switch. Seeking
uses a separate sample-accurate flush, rebuilds filter state, and invalidates queued pre-seek output;
natural EOF drains the active graph. Raw expressions are trusted configuration: validation checks
the PCM graph contract and does not sandbox filters that can access external resources. Master gain
is finite linear `0.0..=2.0`, defaults to unity, and clamps output samples to `[-1, 1]`; gain above
unity can clip.

There are distinct source, filtered-output, and transport timelines. Seeking is relative to the
source timeline. `PlaybackPosition.media` is the source timestamp associated with the latest output
boundary; it is marked estimated when timestamps are missing or a graph buffers or changes timing.
It does not claim that a listener has heard the media. `PlaybackPosition.submitted` after a
time-changing graph measures contiguous filtered PCM samples accepted by the output, not source
time. `PlaybackPosition.played` is only populated by outputs that can measure consumed audio;
Discord currently reports none. Discord's RTP timeline remains connection-owned and continuous
across graph replacement, seek, and source replacement.

`OpusEncoderOutput` owns one playback-scoped packet sink and lazily constructs its libopus encoder
when PCM first arrives. Compatible source packets and encoded PCM use that same sink. Passthrough
eligibility is deliberately narrow: a packet must match the Opus payload's parsed duration, avoid
container trimming, and pass the output's compatibility policy. Filter and gain changes activate
the decoder without reopening the source; bounded preceding packet history warms it. Returning to
passthrough closes partial encoder PCM at a legal packet boundary and discards filter-internal
delayed output. The transition can add a short gap/artifact but does not reset output or RTP state.

The Discord adapter owns a bounded queue and does not block Tokio runtime workers: the synchronous
core worker is the backpressured producer, and the Discord task schedules packets against a
monotonic deadline. An underrun sends Opus silence at the normal cadence. Packets more than one
packet duration late are dropped and the sender rebases its deadline rather than transmitting a
catch-up burst. Pause and end-of-stream drain accepted packets, send the five protocol silence
frames, and turn speaking off. Seek increments an output generation and discards queued pre-seek
packets; RTP sequence, timestamp, and transport nonce state remain owned by the connection and
continue across seeks, processing-mode transitions, and source replacement. Discord currently
cannot report audible sample position, so session `played` remains unknown.

Each Discord connection owns one WebSocket, one UDP socket, one DAVE session, and one sender task.
Playback output instances are connection-scoped producers; creating another output supersedes the
prior producer without disconnecting voice. Voice Gateway v8 Resume is attempted after a WebSocket
close or heartbeat timeout. A rejected Resume fails the connection; the Node adapter does not send
another bot Gateway OP 4 for this native Resume. The outer bot Gateway connection, REST/channel
commands, inbound voice, RTCP, video, Opus receiving, and remote audio upload are not implemented.

## Observability

Rust libraries use `tracing` and remain subscriber-neutral. A native application can use a local
`ObservabilityHandle` for selected Remedia components without changing process-global diagnostics.
`try_install_global()` is explicit, succeeds only when no global subscriber exists, and reports a
typed error instead of replacing an application's subscriber. Since tracing cannot uninstall the
process-global subscriber, an owner that installs it should remain alive until application shutdown
and flush it only after Remedia activity ends. Local handles can coexist with a host subscriber
without taking ownership. The console writer and structured
event queue are bounded and lossy so logging cannot block audio workers. OTLP traces and metrics
require `REMEDIA_OTEL_ENABLED=true`; the default creates no network exporters. Applications should
flush and shut down owned providers during normal application shutdown, never from an audio callback.
See the [observability guide](../apps/docs/content/docs/observability/configuration.mdx) for
environment variables, event fields, metric semantics, and current limitations. Native FFmpeg logs
are routed through one process-wide callback installed once by the core, then emitted through the
active Remedia tracing dispatcher for synchronous Remedia operations. FFmpeg does not expose a
previous-callback getter, so Remedia cannot chain an arbitrary custom native callback; internal
FFmpeg worker-thread messages also cannot always be attributed to the Remedia operation that
created them. These process-global logging limits are documented in the observability guide.

Building `remedia-core` requires the FFmpeg 9 development libraries (libavformat, libavcodec,
libavutil, libavfilter, and libswresample). Cargo builds libopus from the `opus2` bundled source
and statically links the Opus adapter into Remedia binaries, so a separate system libopus development
package is not required. CI installs FFmpeg development/runtime libraries on each platform. No
FFmpeg executable is used at runtime.
