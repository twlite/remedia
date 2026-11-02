# Architecture

This document distinguishes implemented behavior from design intentions. `remedia-core` implements
file, HTTP, buffer, and byte-stream decoding to PCM, playback sessions, generic output, and
transport-independent Opus encoding. `remedia-discord` implements one outbound voice connection with DAVE and RTP-size
transport encryption. The server remains starter code. The N-API package now exposes initial native
playback, Discord session, and diagnostics APIs.

## Rust crates

- **`remedia-core`** opens local filesystem paths, HTTP(S) URLs, owned encoded bytes, and bounded
  byte streams with libavformat, selects and decodes an audio stream with libavcodec, and converts
  output through libswresample to interleaved 48 kHz stereo `f32` PCM. It uses native FFmpeg
  libraries rather than starting FFmpeg subprocesses.
  Playback sessions manage one source, lifecycle state, seeking, optional native libavfilter
  graphs, linear master gain, and frame delivery to a generic output. With no graph configured,
  PCM bypasses libavfilter. The generic Opus output adapter uses native libopus, accumulates variable PCM blocks into
  20 ms stereo Opus packets, and marks a padded final packet's actual media duration. It does not
  pace packets or expose Discord types. Device output is not implemented.
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
- **`remedia-server`** is intended to run the engine as a standalone remote service and expose
  operations over a WebSocket RPC protocol.
- **`remedia-node`** contains the Rust N-API crate and thin TypeScript wrapper. `Remedia` owns
  isolated native playback sessions, Discord voice connections, and one bounded diagnostics
  forwarder. Its TypeScript `DiscordGatewayAdapter` accepts the host application's Gateway send
  function and routes raw `VOICE_STATE_UPDATE`/`VOICE_SERVER_UPDATE` dispatches to one session per
  guild. A join sends OP 4 and waits for both fresh dispatches before invoking the existing native
  credential-based transport API; credentials are not part of the public Node API. A voice server
  or session change requests a fresh Gateway pair before replacing the native transport, stopping
  active playback during replacement. Ordinary voice WebSocket Resume is handled natively and does
  not send a bot Gateway request. Native playback methods run on NAPI-RS's managed Tokio runtime; blocking core methods
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

The core decoder, playback, Opus, outbound Discord transport, and first N-API playback integration
exist. The Node package adapts an externally owned Discord Gateway but does not implement Gateway
connectivity, REST, or channel commands. Server RPC, receiving audio, and device output remain design
intentions. Add crates or packages only when implementation needs them.

`apps/discord` is a manual discord.js integration harness over `@remedia/node`. It forwards raw
Gateway dispatches, sends OP 4 through the owning shard, and provides app-level slash commands with
an in-memory per-guild URL queue. Queueing stays out of the core and Node playback APIs. The bot
runs its TypeScript source with `tsx`; `tsc --noEmit` remains its type check. Its six named audio
presets, `/filter`, and `/volume` commands are harness-only conveniences; the raw graph API remains
generic. The application-owned queue stores per-guild effect settings and preserves them across
skips.

The bounded `DiagnosticReceiver` in `remedia-observability` is connected to the Node wrapper by one
dedicated forwarding worker and a bounded N-API `ThreadsafeFunction`. The receiver is per
`Remedia` context; clones compete for events and are not broadcasts. The wrapper exposes structured
events through `on('debug', listener)` and reports events dropped by either bounded queue. Embedded
contexts disable console output by default to avoid duplicating events handled by JavaScript. The
standalone server can initialize the same shared context when server runtime work begins.

## Embedded and remote integration

Embedded Node.js integration provides local applications direct native access. Buffer and
Uint8Array inputs are copied into native-owned memory. A Node Readable is pumped incrementally into
a 512 KiB bounded queue, so decoder backpressure reaches the producer without creating an
unbounded audio buffer. Cancelling playback releases queued bytes and detaches the feeder; the
caller retains ownership of the Readable, which is paused but not destroyed.

Remote integration runs in a separate server process. Its intended initial inputs are files and
HTTP URLs that the server can access. Uploading arbitrary client-generated binary streams to the
remote server is intentionally deferred; the local N-API byte input does not add remote transport.

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

Each `PlaybackSession` owns one blocking worker, decoder, and output instance. It applies direct
backpressure by synchronously handing one owned frame at a time to `MediaOutput`; it has no
session-level audio queue. Pause waits for an in-flight read or output call to reach a frame boundary.
Stop cancels the decoder/output and joins the worker. `NullOutput` records consumption statistics
without real-time pacing. The session distinguishes the position submitted to an output from an
optional output-reported played position.

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

`OpusEncoderOutput` owns one libopus encoder and one packet sink per playback. The Discord adapter
owns a bounded queue and does not block Tokio runtime workers: the synchronous core worker is the
backpressured producer, and the Discord task schedules packets against a monotonic deadline. An
underrun sends Opus silence at the normal cadence. Packets more than one packet duration late are
dropped and the sender rebases its deadline rather than transmitting a catch-up burst. Pause and
end-of-stream drain accepted packets, send the five protocol silence frames, and turn speaking off.
Seek increments an output generation and discards queued pre-seek packets; RTP sequence, timestamp,
and transport nonce state remain owned by the connection and continue across seeks and playback
replacement. Discord currently cannot report audible sample position, so session `played` remains
unknown while `submitted` tracks decoded frames accepted by the Opus output.

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

Building `remedia-core` requires native libavformat, libavcodec, libavutil, libswresample, and
libopus development libraries. CI installs these libraries on Ubuntu. No FFmpeg executable is used
at runtime.
