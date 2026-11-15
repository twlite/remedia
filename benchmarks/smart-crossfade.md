# Smart crossfade scheduling

Run with the repository's pinned toolchain and FFmpeg 9 development/runtime libraries selected:

```sh
cargo run --release -p remedia-core --example crossfade_scheduling
cargo test -p remedia-core
cargo run --release -p remedia-core --example concurrent_playback_benchmark -- crates/remedia-core/tests/fixtures/generated-opus-passthrough.ogg 32 passthrough unpaced
cargo run --release -p remedia-core --example concurrent_playback_benchmark -- crates/remedia-core/tests/fixtures/generated-opus-passthrough.ogg 32 crossfade unpaced
```

The scheduling example uses the implementation's analyzer directly. It creates a deterministic
20-second stereo signal (ten seconds of audio, ten seconds of silence), then analyzes it 500 times.
The allocator counts construction separately from processing; fixtures, printing, and OS resource
collection are outside allocation measurement. CPU is native process user plus system time.
The example asserts zero processing allocations and a conservative endpoint within 250–800 ms after
the audible signal ends. This is a confidence guard, not a perceptual quality score.

Local release run on macOS arm64, Rust 1.99.1, FFmpeg 9 (2026-11-15):

- 10,000 seconds analyzed in 337.245 ms wall time, 337.038 ms CPU time.
- 0.703 ns per stereo frame, one 41,600-byte statistics allocation per probe,
  zero processing allocations.
- Selected endpoint 10,610 ms: 610 ms after the generated audible ending, including smoothing
  and the 250 ms safety margin.
- The separate controlled-sink passthrough runs both submitted 3,232 packets / 64.64 seconds
  of audio with 932,096 bytes of aggregate packet capacity. Crossfade enabled without a next
  stage starts no probe and preserves every encoded source packet.

These are single local runs on a shared host. The microbenchmark excludes FFmpeg opening, seeking,
decoding, output encoding and thread startup. The controlled-sink run measures packet preservation,
not real Discord transport. Do not infer production CPU costs from the analyzer-only result.

The native probe decodes at most the configured fade plus 15 seconds near EOF, with an additional
hard read cap of 51 seconds for inconsistent input metadata. It retains hop powers rather than PCM.
It never probes network sources. Leading inspection uses existing staged PCM and holds at most two
seconds plus one decoder block before deciding; the existing two-second stage applies backpressure.

Deterministic unit/native tests cover silent and noise-floor tails, short pauses, returning audio,
quiet outros, quiet ambient signals, exponential fade/reverb, malformed/cancelled analysis,
chunk invariance, bounded analysis, leading padding, quiet intros, unknown durations and HTTP
fallback. Playback tests exercise both curves, trimmed incoming progress, smart overlap timing,
continuous output samples, late/failed staging, pause/seek/stop, consecutive promotions and Opus
passthrough. Synthetic fixtures cannot establish listening quality across real music libraries.
