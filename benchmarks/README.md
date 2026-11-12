# Concurrent playback measurements

This is the first measured implementation stage of the concurrency redesign.
It fixes native filter pool multiplication, reuses native resampling output,
and shrinks owned Opus packet allocations. It does **not** implement the full
requested scheduler or adaptive crossfade redesign.

See [adaptive-crossfade.md](adaptive-crossfade.md) for the crossfade curve comparison and mixer cost, which
has its own example and does not use the runner below.

## Reproduction

```sh
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig cargo build --release -p remedia-core --example concurrent_playback_benchmark
python3 benchmarks/playback.py --output benchmarks/after.jsonl
python3 benchmarks/summarize.py
```

The pkg-config override is specific to this machine. Run the same example and
runner at base commit `0079b248115b` (copying in the benchmark harness) to reproduce
the baseline. Preserve the base executable and pass `--binary /path/to/base` to
compare builds. The measurements checked in here used a preserved baseline
executable built from the original core sources. The changed build contains the
filter/resampler/packet changes together; their CPU effects are not isolated.
`--complexity 5` benchmarks an explicit alternative to the default 9; output
quality must be evaluated separately before choosing another setting.

`baseline.jsonl` and `after.jsonl` contain raw measurements. `results.md` contains
medians for every workload and checks packet/sample totals agree between builds.
These runs use generated checked-in Opus (2.02 s) and MP3 (~0.31 s) fixtures;
no media processing subprocess, public network, or Discord credentials are used.

## Environment and interpretation

Measured locally on Apple M5, 10 logical CPUs, 24 GiB RAM, macOS Darwin 27 arm64,
Rust 1.99.1, release profile, native FFmpeg selected from Homebrew's `ffmpeg`
pkg-config directory. Opus complexity 9, 128000 bits/s, VBR in both builds.
There are three repetitions of 1/8/32/64/128 sessions, automatic passthrough,
volume, and tremolo filtering, paced and unpaced, for both fixtures.
The host was not isolated; other verification activity overlapped some baseline
runs. Treat timing and CPU changes as exploratory, not statistically established
performance gains. Thread count and allocation capacity changes have direct
structural explanations and regression tests.

The runner samples the process at approximately 20 ms intervals using macOS
`proc_pidinfo` or Linux `/proc`; fast unpaced workloads may escape peak sampling.
CPU is child process user+system time including startup and teardown. RSS is
process resident memory rather than allocator live bytes or memory per session.
The packet capacity metric sums capacities of all packets transferred over the
run; it is not peak retained memory. The controlled sink has no queue and blocks
until the packet deadline. It neither measures nor changes Discord RTP pacing.
Startup begins just before each `play`; sinks have independent deadlines and
sessions start sequentially. The program fails on playback errors, and the runner
kills a workload after 60 seconds. Use the example directly with longer fixtures
for sustained load; the short committed runs are not production qualification.

## Investigation and retained constraints

- Core workers call synchronous FFmpeg input and decode operations, then native
  filters, gain, and synchronous `MediaOutput` writes. Node and server both use
  this core. Node dispatches blocking calls off its async executor.
- libavfilter's default per-graph pool caused 128 filtered sessions to create
  1409 threads on this host. One graph thread keeps filtering on the existing
  playback worker. No filter or encoder quality setting was reduced.
- Each encoded packet previously retained a 4000-byte Vec, even after truncation.
  Encoding now uses bounded stack scratch and copies the actual compressed bytes
  into the owned transport packet. This trades a small payload copy for smaller
  allocations; it does not eliminate the required owned packet allocation.
- Resampling previously created a fresh native output frame and referenced the
  decoded frame on every conversion. Native output storage is now reused and
  only frames missing channel layout need a temporary reference. Owned interleaved
  PCM remains necessary for the existing output API.
- `OpusEncoderOutput` serializes encoder state and sink operations. Dropping that
  guard without another ordering barrier would race flush, finish, and packet
  writes. Cancellation already reaches the sink without taking this guard.
- Discord uses bounded packet channels and a paced async actor, with separate
  source and transport generations. Transport migration can park synchronous
  output. DAVE encryption and RTP deadlines were left intact.
- Crossfade has a dedicated PCM loop, bounded staged PCM, and synchronous joins.
  Its prepared producer owns its decoder after promotion. Arbitrary return to
  compressed packets would bypass already decoded samples and codec state.
- No shared CPU scheduler was substituted: the current blocking loop is unsuitable
  for a bounded CPU pool. This change does not demonstrate that a candidate
  readiness-driven scheduler improves performance, and provides no such claim.

## Work still required for the full redesign

Adaptive crossfade passthrough with preroll and exact compressed boundaries;
readiness-driven bounded CPU scheduling with separate cancellable source I/O and
output handoffs; prompt stage retirement with deterministic final joins; per-stage
CPU/I/O/allocation metrics; AAC and repeated-crossfade performance fixtures; stalled
HTTP and command-churn performance measurements; queue occupancy, underruns, drops,
and real RTP/DAVE latency; sustained hundreds-session qualification. Existing
regression tests cover several of these behaviors, but those tests are not
performance measurements or evidence that the remaining redesign is implemented.

## Phase 2

Adaptive playback, native resource collection, longer command workloads, and the
new measurements are documented in [phase2.md](phase2.md). Original stage-one
results above are preserved.
