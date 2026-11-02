# remedia-core

`remedia-core` is Remedia's native media engine crate. It opens local files and HTTP(S) URLs with
FFmpeg's native libraries, demuxes audio incrementally, decodes as required, and normalizes PCM to
interleaved 48 kHz stereo `f32`. It also provides playback sessions, generic PCM output, and a
transport-independent Opus packet output.

## Boundaries

- `AudioDecoder` handles input, stream selection, decoding, resampling, metadata, and seeking.
- `PlaybackSession` owns one demux reader, a lazy decoder/resampler when PCM is needed, and one
  output, including pause, resume, cancellation, and source/output position tracking.
- `MediaOutput` consumes owned PCM frames. `NullOutput` is provided for tests and benchmarks.
- `OpusEncoderOutput` optionally accepts compatible source Opus packets or encodes variable-sized
  PCM frames to owned Opus packets for the same `OpusPacketSink`. Packet passthrough is restricted
  to untrimmed compatible 48 kHz mono/stereo Opus with no required processing. Filtergraph or
  volume changes can switch modes without reopening the source; unsupported inputs fall back to PCM.
  The adapter does not pace packets or depend on a transport.
- `PlaybackSession::with_observability` attaches lifecycle events and metrics to an application-owned
  Remedia observability context. The core never installs a global subscriber.

Discord networking belongs to `remedia-discord`; this crate has no Discord dependency.

## Native prerequisites

Building requires development headers and libraries for FFmpeg's `libavformat`, `libavcodec`,
`libavutil`, and `libswresample`. The Opus output also requires native `libopus` development files.
Supported codecs depend on the FFmpeg build available on the host.

## Development

Run these commands from the repository root:

```sh
cargo test -p remedia-core
cargo clippy -p remedia-core --all-targets -- -D warnings
cargo run -p remedia-core --example decode -- file ./media/track.wav
cargo run -p remedia-core --example playback -- file ./media/track.wav
cargo run -p remedia-core --example opus_processing_benchmark -- ./media/track.ogg
```

See the [decoding guide](../../apps/docs/content/docs/core/decoding.mdx) and
[playback guide](../../apps/docs/content/docs/core/playback.mdx) for public API examples and
behavior details. See the
[observability guide](../../apps/docs/content/docs/observability/configuration.mdx) for shared
tracing, event routing, and OTLP setup.
