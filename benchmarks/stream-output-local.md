# Local streaming benchmark run

These are debug-build smoke measurements on macOS Darwin 27.0.0 arm64, Node 24.22.1,
Rust 1.99.1 and Homebrew FFmpeg 9.0.2. They validate the harnesses and expose raw costs;
they are not release-performance acceptance results. Other build/test activity overlapped parts
of the Node run, and startup dominates the short inputs. Do not infer small performance differences.

- [Node measurements](stream-output-local.jsonl): three repetitions of one-second generated stereo
  PCM input at 1 and 4 concurrent sessions. NullOutput is the baseline; s16le, f32le and Opus each
  have fast, slow and stalled consumers. There are 60 rows. Slow consumers use realtime pacing;
  stalled consumers are cancelled after 250 ms. Rows report CPU, throughput, delivery lag,
  JS heap/external memory, RSS, cached native process counters and OS threads.
- [Rust allocator measurements](stream-output-allocations-local.jsonl): three repetitions per
  NullOutput/s16le/f32le/Opus with `generated-tone.mp3`, 12 rows. Each runs startup, playback and
  teardown. Allocation counters include Rust allocation/reallocation calls and requested bytes,
  excluding FFmpeg/libopus C allocations and JavaScript. Opus uses the default encoder settings.

Reproduce with the commands in [README](README.md#standalone-stream-output). The local run used
`cargo build` rather than `cargo build --release`; rerun release builds for performance decisions.
The Node command used `STREAM_SECONDS=1 STREAM_REPETITIONS=3 STREAM_CONCURRENCY=1,4`.
The Rust example used the default debug executable and three repetitions for each format.

Native linking required local environment overrides:

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig
export RUSTFLAGS='-l c++ -L native=/opt/homebrew/opt/openssl@4/lib -L native=/opt/homebrew/opt/svt-av1/lib'
```

The linker also needed `LIBRARY_PATH` pointing at installed Homebrew dependency library directories.
`FFMPEG_DIR` was unset. This allowed runtime tests, but the addon links external media/TLS libraries
and fails the repository's static-distribution audit. These overrides are not shipping build changes.

Process memory includes V8, Rust, native codecs, stacks and allocator caches. Cached resource samples
can miss brief thread and memory peaks. The three short lifecycle repetitions do not establish
long-term leak freedom. Native C allocator profiling, long-duration churn, and larger concurrency
runs remain necessary before publishing resource-efficiency claims. The Rust counters quantify
allocator traffic, not isolated native live heap. PCM serialization bytes and packet-pointer tests
identify explicit payload copying; they do not measure every internal codec copy.
