# Architecture

This document describes the current boundaries and the design still to be implemented. `remedia-core`
implements file/HTTP-to-PCM decoding, playback sessions, and a generic media output interface.
Discord transport and the server remain starter code. The N-API package has a minimal smoke export
but is not yet connected to the engine.

## Rust crates

- **`remedia-core`** opens local filesystem paths and HTTP(S) URLs with libavformat, selects and
  decodes an audio stream with libavcodec, and converts output through libswresample to interleaved
  48 kHz stereo `f32` PCM. It uses native FFmpeg libraries rather than starting FFmpeg subprocesses.
  Playback sessions manage one source, lifecycle state, seeking, and frame delivery to a generic
  output. Device output, filtering, and transport scheduling are not implemented yet.
- **`remedia-discord`** is intended to adapt the generic engine's output to Discord voice transport.
  Discord-specific behavior should remain outside the generic media engine.
- **`remedia-server`** is intended to run the engine as a standalone remote service and expose
  operations over a WebSocket RPC protocol.
- **`remedia-node`** contains the Rust N-API crate. It is intended to call the engine directly in the
  Node.js process instead of routing local calls through the standalone server; the current smoke
  export does not yet provide that integration.

The core decoder and playback APIs exist; the remaining component boundaries are design intentions.
Add crates or packages only when implementation needs them.

## Embedded and remote integration

Embedded Node.js integration is intended for local applications that need direct native access.
It may ingest client-provided streams or buffers because the binding and the producer run in the
same process.

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
optional output-reported played position. No global scheduler or audio-device output is implemented.

Building `remedia-core` requires native libavformat, libavcodec, libavutil, and libswresample
development libraries. CI installs these libraries on Ubuntu. No FFmpeg executable is used at
runtime.
