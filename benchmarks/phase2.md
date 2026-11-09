# Phase 2 performance and resource verification

The first-stage filter thread cap, native resampling reuse, packet allocation
changes, original raw measurements, and original report remain intact.

## Implemented changes

- Crossfade policy no longer forces direct eligible Opus sources through PCM.
  Whole compressed packets are forwarded before overlap. A packet that would
  cross the overlap boundary is retained, the decoder warms from bounded history,
  and PCM is trimmed to the exact boundary before sample-accurate equal-power mixing.
  If preparation is unavailable at the decision point, ordinary passthrough can
  continue to EOF. Existing source identity, expiry, generations, events, and
  one-output promotion semantics remain in place.
- Direct sources can return to passthrough after temporary volume processing
  when decoded buffers are empty and source and encoder sample boundaries agree.
  Seeking disables restoration. Filters and incompatible packets retain the
  existing transcoding fallback. Promoted staged producers remain in PCM: they
  still own the decoder and bounded buffer. No source is reopened to obtain packets.
- Stop signals active decoder and output cancellation before staged joins.
  Joins remain deterministic; no detached reaper or unbounded retirement queue
  was introduced. Existing native cancellation interrupts stalled HTTP I/O.
- A command-churn benchmark exposed a startup race: immediate pause could change
  Playing to Pausing before the worker woke, causing the worker to exit and leave
  pause waiting. The startup handshake now admits active pause/resume states;
  a regression test covers immediate pause, and the formerly stalled workload completes.
- Native process resources are cached off the calling thread for server, discovery,
  and embedded instances. See the process resource documentation for precise OS
  support, null fields, units, and demand-driven CPU sampling.

## Measured results

[Full measurement tables](phase2-results.md) retain every tested count and latency
column; raw data are in the `phase2-*.jsonl` files. The before build used first-stage
memory/filter changes and the original crossfade loop, with the same benchmark
crossfade selector added. Both comparison builds used complexity 9, 128000 bits/s,
and VBR. The initial comparison harness had no native resource-monitor thread;
RSS and OS threads were sampled externally through native libproc, not a shell.

At 128 sessions the three-run CPU median changed from 2.283 to 1.031 seconds;
sampled RSS changed from 85152 to 35872 KiB and thread count stayed 129. At 512
sessions CPU changed from 11.159 to 6.329 seconds and RSS from 298912 to 102608 KiB.
Packet and delivered-sample totals match for every comparison. This workload
measures avoiding unnecessary transcoding when crossfade is enabled without an
active overlap. It does not measure continuous crossfade mixing throughput.

The 15-second filtered runs with pause/seek/resume completed at all counts through 512. They emit actual native resource samples each second, including RSS, macOS
physical footprint, CPU times/utilization, threads, descriptors, and collection cost.
Private resident memory remains null on macOS; footprint is labeled separately.
The largest observed collection cost was approximately 0.123 ms in these runs.

Sink p99 was noisier at some counts, including 6.603 ms in the 256-session longer
run. Other verification activity overlapped measurements; the host was not isolated.
These samples support the measured CPU and allocation-path improvement, not a claim
of universally improved deadlines or production qualification. Startup in command
runs includes deliberate pause time while other sessions initialize. Command latency
combines immediate pause, paused seek, and resume; it does not measure seek during
blocked network I/O or every skip/stop scenario. Controlled sink delay is not RTP delay.

## Reproduction

Use native FFmpeg libraries selected by your installation. This host used
`PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig`, Apple M5 / 10 logical CPUs /
24 GiB RAM / Darwin 27 arm64, and Rust 1.99.1.

```sh
cargo build --release -p remedia-core --example concurrent_playback_benchmark
python3 benchmarks/playback.py --output /tmp/phase2-opus.jsonl --counts 1,8,32,64,128,256,512 --repeats 3 --workloads crossfade --fixtures generated-opus-passthrough.ogg --pacing paced
python3 benchmarks/make_sustained_fixture.py /tmp/remedia-sustained.wav --seconds 15
python3 benchmarks/playback.py --output /tmp/phase2-long.jsonl --counts 1,8,32,64,128,256,512 --repeats 1 --workloads filter --fixtures /tmp/remedia-sustained.wav --pacing paced --controls
python3 benchmarks/summarize_phase2.py
```

The current harness also emits native resource samples; its extra sampler thread
must be accounted for when comparing with the original short-run raw files.
Use `--binary PATH` for a preserved baseline executable, and preserve identical
harness instrumentation in any new comparison. The runner times out after 60 seconds
per case; use the example directly for longer fixtures. Files and fixture generation
use no media subprocesses, public services, or Discord credentials.

## Verification boundaries and retained design

Rust tests cover byte-stream backpressure/cancellation, blocked HTTP opening,
independent sessions, repeated promotions and invalidation, exact packet equality
without overlap, compressed-to-PCM overlap sample accounting, direct PCM-to-Opus
restoration, and stopping during stalled HTTP preparation. Native/client tests
cover shared-process snapshots, null accounting, and older-server compatibility.
The CLI JSON discovery test checks the additive resource structure. Zod and Rust
share invalid/valid cases and default expansion; CI verifies generated artifacts.

The worker architecture is retained. Paced playback remains approximately one
worker per active session; blocking source I/O and output waits remain isolated
per session. No comparative evidence for a different CPU scheduler was established,
so no scheduler was substituted. Per-stage CPU versus FFmpeg I/O attribution,
output-wait duration, and queue occupancy are not yet instrumented. Real Discord
voice/DAVE integration and long-duration continuous promoted Opus passthrough are
not verified by these controlled runs. Linux and Windows resource code passed
cross-target Clippy checks; native runtime resource accuracy was exercised on macOS
only. Unsupported counters and platforms return null rather than approximations.
