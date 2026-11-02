# remedia-core

`remedia-core` is Remedia's native media engine crate. It opens local files and HTTP(S) URLs with
FFmpeg's native libraries, decodes audio incrementally, and normalizes it to interleaved 48 kHz
stereo `f32` PCM. It also provides playback sessions, generic PCM output, and a transport-independent
Opus encoder output.

## Boundaries

- `AudioDecoder` handles input, stream selection, decoding, resampling, metadata, and seeking.
- `PlaybackSession` manages one decoder and one output, including pause, resume, cancellation, and
  position tracking.
- `MediaOutput` consumes owned PCM frames. `NullOutput` is provided for tests and benchmarks.
- `OpusEncoderOutput` converts variable-sized PCM frames to owned 20 ms Opus packets and forwards
  them to an `OpusPacketSink`. It does not pace packets or depend on a transport.

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
```

See the [decoding guide](../../apps/docs/content/docs/core/decoding.mdx) and
[playback guide](../../apps/docs/content/docs/core/playback.mdx) for public API examples and
behavior details.
