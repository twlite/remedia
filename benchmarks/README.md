# Concurrent playback measurements

This is the first measured implementation stage of the concurrency redesign.
It fixes native filter pool multiplication, reuses native resampling output,
and shrinks owned Opus packet allocations. It does **not** implement the full
requested scheduler or adaptive crossfade redesign.

See [adaptive-crossfade.md](adaptive-crossfade.md) for the crossfade curve comparison and mixer cost, which
has its own example and does not use the runner below.

See [smart-crossfade.md](smart-crossfade.md) for content-aware scheduling accuracy, CPU and allocation measurements,
and the passthrough preservation workload.

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

## Phase 3: resource pass (pre-skip, seek and promotion recovery, packet path, LTO)

Everything here is reproducible from the repository plus FFmpeg's command-line tool, which is used
**only to generate fixtures once**; the engine and the benchmark never spawn it.

### Fixtures

```sh
mkdir -p /tmp/remedia-fx && cd /tmp/remedia-fx
# 60 s of deterministic music-like stereo (tones, vibrato, pseudo-random noise)
ffmpeg -f lavfi -i "aevalsrc=0.2*sin(2*PI*220*t)*sin(2*PI*0.5*t)+0.15*sin(2*PI*330*t+sin(2*PI*3*t))+0.1*sin(2*PI*(440+30*sin(0.3*t))*t)|0.2*sin(2*PI*277*t)*sin(2*PI*0.7*t)+0.15*sin(2*PI*392*t)+0.1*random(0)-0.05:s=48000:d=60" -c:a pcm_s16le src.wav
ffmpeg -i src.wav -c:a libopus -b:a 128k music60.opus        # real file: pre-skip 312
ffmpeg -i src.wav -c:a libmp3lame -b:a 192k music60.mp3
ffmpeg -i src.wav -c:a aac -b:a 128k music60.m4a
ffmpeg -t 15 -i music60.opus -c copy music15.opus            # real, 15 s, for crossfade transitions
# Files rewritten to pre-skip 0 (what the original phase-2 benchmarks measured):
python3 benchmarks/make_opus_fixture.py music60.opus music60-pt.opus
python3 benchmarks/make_opus_fixture.py music15.opus music15-pt.opus
```

### Build variants

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig   # this host; adjust elsewhere
for mode in off thin fat; do
  CARGO_PROFILE_RELEASE_LTO=$mode CARGO_TARGET_DIR=/tmp/tgt-$mode \
    cargo build --release -p remedia-core --example concurrent_playback_benchmark
done
# The base commit with fat LTO, to separate the LTO effect from the code changes:
git worktree add /tmp/wt-base 0491ffd   # harness commit; the engine is unchanged from b690b33
(cd /tmp/wt-base && CARGO_PROFILE_RELEASE_LTO=fat CARGO_TARGET_DIR=/tmp/tgt-basefat \
   cargo build --release -p remedia-core --example concurrent_playback_benchmark)
```

### Running

`resource_matrix.py` runs each case in a fresh process and records `wait4` CPU and peak RSS
(exact, per process), the in-process one-second resource samples (steady-state CPU, macOS
physical footprint, thread count), the share of time the sessions spent in passthrough, and sink
delay percentiles. `--alloc` adds Rust heap counters in separate rows.

```sh
python3 benchmarks/resource_matrix.py --binary /tmp/tgt-fat/release/examples/concurrent_playback_benchmark \
  --fixtures /tmp/remedia-fx --label after-fat --repeats 3 --output after-fat.jsonl
python3 benchmarks/summarize_resources.py before.jsonl after.jsonl --title "Title"
```

Cases (all paced, 20 s unless noted): `opus-passthrough`, `opus-crossfade-idle`, `opus-volume`,
`opus-seek` (a random seek per session every 3 s, 30 s), `opus-transition` (two crossfade promotions
per session, 36 s), the `opus-real-*` variants of the same on real files, `mp3-transcode`,
`aac-transcode`, `mp3-filter` and `mp3-stream` (a bounded byte-stream producer like a plugin).
The example's allocation counter is enabled with `alloc`; it counts Rust allocations only, because
FFmpeg and libopus allocate natively.

Packet-path microbenchmarks (RTP datagram construction and routed delivery) are ignored tests:

```sh
cargo test --release -p remedia-discord -- packetization_cost routed_delivery_cost --ignored --nocapture --test-threads=1
```

Results and their interpretation are in [phase3.md](phase3.md).

## Plugin media transfer and queue audit

The added `ByteStreamWriter::write_bytes` path removes the server's `Bytes::to_vec()` ingress
allocation for fitting response chunks. [plugin-media-transfer.md](plugin-media-transfer.md)
records the identical-sample decoder check, producer-side allocation counts, separate queue
occupancy/RSS/thread measurements, reqwest/Hyper chunk ownership review, and cancellation tests.
The pending-byte cap is not a strict retained-memory bound for caller-owned `Vec` capacity or
shared `Bytes` backing allocations.

## Server lifecycle resource audit

Agent 6's repeated client/guild create, detach, resume, close, and expiry-waiter measurement is
documented in [resource-audit-6.md](resource-audit-6.md), with raw runs in
[`data/resource-audit-6.jsonl`](data/resource-audit-6.jsonl). It uses an ignored `remedia-server`
unit workload rather than the concurrent playback matrix.

## Standalone stream output

Build `@remedia/node` with the same native release configuration for every run, then:

```sh
STREAM_SECONDS=3 STREAM_REPETITIONS=3 STREAM_CONCURRENCY=1,8,32 \
  node --expose-gc --experimental-strip-types benchmarks/stream-output.ts > stream-output.jsonl
```

The harness generates stereo WAV bytes in-process and compares `NullOutput` with s16le, f32le and
Opus. Fast consumers measure throughput; slow consumers use realtime pacing and await each chunk;
stalled consumers remain unread for 250 ms before cancellation. Every row creates/disposes all
sessions, so repeated rows also expose lifecycle churn. Use at least three repetitions in a quiet
process and compare medians at matching concurrency. Preserve machine, compiler, FFmpeg identity
and native release flags alongside results. No competing projects or external services are used.

JSONL includes process CPU ms per audio second, audio seconds per wall second, maximum delivery
lag relative to the output sample clock, payload/chunk counts, PCM serialization bytes, JS heap and
external-memory peaks, RSS, post-GC memory and cached native process resource/thread measurements.
The 25 ms sampler and native cache can miss brief peaks; native snapshots describe the whole Node
process, not an isolated Rust heap. Stalled rows deliberately have no throughput/CPU-per-audio
result because they do not finish their input. Subtract comparable null rows to estimate streaming
overhead. Payload counts and PCM serialization bytes quantify explicit payload work, **not total
allocator calls**. Use the platform allocator profiler to measure allocation stacks and native
heap separately; RSS minus JS heap does not establish Rust heap consumption. The harness itself
adds sampling and per-chunk bookkeeping overhead. No performance claim is made without runtime
results. Longer repetitions are needed to assess allocator-cache stability versus leaks.

For Rust allocator traffic independently of V8 and NAPI, run the companion example for each output:

```sh
cargo run --release -p remedia-core --example stream_output_benchmark -- \
  crates/remedia-core/tests/fixtures/generated-tone.mp3 null 3
cargo run --release -p remedia-core --example stream_output_benchmark -- \
  crates/remedia-core/tests/fixtures/generated-tone.mp3 s16le 3
```

Repeat with `f32le` and `opus`, and with the generated Opus fixture to measure passthrough. It uses
the existing benchmark's Rust system-allocator instrumentation and reports allocation/reallocation
calls and requested bytes during startup, playback and teardown. This excludes FFmpeg/libopus C
allocator calls and V8. Byte serialization counts describe the explicit PCM conversion; Opus packet
ownership transfer is also tested by pointer identity. The benchmark's blocking executor parks its
calling thread on a waker and creates no output bridge thread.

## Experimental hybrid playback scheduler

The local experiment is described in [hybrid-scheduler.md](hybrid-scheduler.md). It preserves the
original main release binaries separately, compares the extracted dedicated driver, and uses
`hybrid.py` for interleaved dedicated/pool comparisons with delivered-audio checks. Build with
`hybrid-build.sh`; raw rows, binary/fixture manifests and check summaries are under `data/hybrid`.
The feature remains opt-in and the report lists unqualified workloads and measurements.

The continuation's three-way comparison, processing budgets, pacing deadlines and source/transport
investigation are in [hybrid-ab-results.md](hybrid-ab-results.md). Use `hybrid-ab-build.sh` and
`hybrid_ab.py`; matched rows and separate diagnostic/fallback records are under `data/hybrid/ab`.
`hybrid-profile-build.sh` instruments an archived copy without changing normal performance builds.
Startup diagnostics are explicitly excluded from scheduler performance evidence.
