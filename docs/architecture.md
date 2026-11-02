# Architecture

This document distinguishes implemented behavior from design intentions. `remedia-core` implements
file/HTTP-to-PCM decoding, playback sessions, generic output, and transport-independent Opus
encoding. `remedia-discord` implements one outbound voice connection with DAVE and RTP-size
transport encryption. The server remains starter code. The N-API package now exposes initial native
playback, Discord session, and diagnostics APIs.

## Rust crates

- **`remedia-core`** opens local filesystem paths and HTTP(S) URLs with libavformat, selects and
  decodes an audio stream with libavcodec, and converts output through libswresample to interleaved
  48 kHz stereo `f32` PCM. It uses native FFmpeg libraries rather than starting FFmpeg subprocesses.
  Playback sessions manage one source, lifecycle state, seeking, and frame delivery to a generic
  output. The generic Opus output adapter uses native libopus, accumulates variable PCM blocks into
  20 ms stereo Opus packets, and marks a padded final packet's actual media duration. It does not
  pace packets or expose Discord types. Device output and filtering are not implemented.
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
  forwarder. Native playback methods run on NAPI-RS's managed Tokio runtime; blocking core methods
  are dispatched to blocking workers. A caller-owned cancellation token covers the gap before a
  scheduled decoder worker starts, so concurrent stop, dispose, or close cannot leave a late
  playback worker behind. Standalone sessions currently decode into `NullOutput`,
  while Discord sessions use the core Opus output adapter. The addon does not install a global
  tracing subscriber. It is private and under development; it does not accept Node streams or
  buffers and does not implement server RPC.

The core decoder, playback, Opus, outbound Discord transport, and first N-API playback integration
exist. Server RPC, receiving audio, and device output remain design intentions. Add crates or
packages only when implementation needs them.

The bounded `DiagnosticReceiver` in `remedia-observability` is connected to the Node wrapper by one
dedicated forwarding worker and a bounded N-API `ThreadsafeFunction`. The receiver is per
`Remedia` context; clones compete for events and are not broadcasts. The wrapper exposes structured
events through `on('debug', listener)` and reports events dropped by either bounded queue. Embedded
contexts disable console output by default to avoid duplicating events handled by JavaScript. The
standalone server can initialize the same shared context when server runtime work begins.

## Embedded and remote integration

Embedded Node.js integration is intended for local applications that need direct native access.
Future local bindings may ingest client-provided streams or buffers because the binding and producer
run in the same process; that input path is not part of the current Node API.

Remote integration runs in a separate server process. Its initial inputs are files and HTTP URLs
that the server can access. Uploading arbitrary client-generated binary streams to the remote
server is intentionally deferred; it adds a transport and lifecycle surface that local bindings do
not need.

## Processing and inputs

The implemented decoder accepts explicit file-path and HTTP(S)-URL source variants. It probes the
container, chooses an audio stream, and yields frames incrementally as fast as the caller consumes
them. FFmpeg handles redirects and unknown HTTP content lengths. A configurable network I/O timeout
and an atomic cancellation token can interrupt blocked operations. The decoder itself never paces
audio.

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
close or heartbeat timeout. A rejected Resume fails the connection; the caller must obtain fresh
voice session information and connect again. The outer bot Gateway, REST/channel joining, inbound
voice, RTCP, video, Opus receiving, and remote audio upload are not implemented.

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
environment variables, event fields, metric semantics, and current limitations.

Building `remedia-core` requires native libavformat, libavcodec, libavutil, libswresample, and
libopus development libraries. CI installs these libraries on Ubuntu. No FFmpeg executable is used
at runtime.
