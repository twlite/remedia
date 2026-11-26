# Hybrid scheduler: transport and control eligibility

Continuation of [the matched release comparison](hybrid-ab-results.md). Previous binaries,
manifests and raw results are retained unchanged. Dedicated execution remains the production
default; all selection and readiness APIs below require `experimental-scheduler`.

## Discord transport boundary

`OpusEncoderOutput` now opts in when its sink implements nonblocking packet admission and
resumable barriers. `DiscordOpusSender` and `RoutedOpusSender` implement this capability. Codec
workers use `try_send` on the existing three-packet transport channel. Registering readiness
before attempting admission avoids a drain/register lost wake. The actor wakes the producer
when it removes a channel packet; actor exit, cancellation, sender replacement and router
attachment also release waits. The router retains rejected packet ownership across migration;
accepted old-transport packets keep the existing discard-on-migration semantics.

The encoder retains one owned PCM frame and feeds the same encoding routine in 960-sample
slices. It retains at most two encoded packets (finalization can produce two), never an
unbounded encoded burst. This is still the existing codec pipeline: bitrate 128000,
complexity 9, VBR, passthrough/recovery and encoder alignment are unchanged. Dedicated outputs
continue to call their synchronous sink directly. No transport state is moved to the CPU pool.

Control acknowledgements use bounded flume channels, which support both the original blocking
receiver and a wake-driven future. A scheduled sender retains one pending barrier future;
subsequent polls do not resubmit it. Pause and resume publish playback state only after its
acknowledgement. Processor EOF is finalized once, and playback completes only after the actor
acknowledges the finish barrier. The actor's RTP queue, deadlines, silence tail, packetizer,
DAVE, encryption and connection migration logic remain in the transport crate.

Validation at this milestone: core + Discord tests pass with the feature enabled and disabled,
including deterministic full-queue ownership, one-time barrier submission, detached-router
wake-up, owned-memory scheduled PCM, sample continuity and final sample counts. A large PCM
frame produces byte-identical encoded packets with dedicated and scheduled output. Existing
RTP/transport/DAVE loopback decryption tests pass. Clippy for both affected crates/all targets
passes with the feature enabled. These tests do not establish live Discord delivery or
constrained-Linux performance.

## Measurement environments

The final macOS suite uses Apple M5 (10 logical CPUs, 24 GiB), macOS 27.0.1 arm64,
Rust 1.99.1 and Homebrew FFmpeg 9.0.2. All variants share native libraries, release
profile, fixtures, 128 kbit/s VBR Opus and complexity 9. `environment-final.json`
records compiler and native ABI versions. The machine remains an interactive desktop;
OS/background activity is a limitation even with sequential, interleaved child runs.

Docker Desktop supplies Ubuntu 24.04.5 arm64 on Linux 7.0.14-linuxkit. Initial Docker
Hub DNS failure prevented pulling the Rust image. A cached Ubuntu image with explicit
DNS allowed installing the compiler and building the pinned repository FFmpeg 9.0.2 /
OpenSSL static recipe. Final variants reuse the same audited native archive prefix;
archive hashes, source commits and configuration are in `environment-linux-final.json`.
The benchmark build uses static pkg-config private dependencies, including OpenSSL.
It does not use the abbreviated `FFMPEG_DIR` library list, which failed linking.
Builds use a detached Git copy on the container filesystem. The live host bind mount
exposed a truncated source page during a test build (host 320 lines, container 268);
that build produced no accepted measurements. The fixed snapshot removes that source
ambiguity and is now part of the reproducible Linux build script.

Linux runs set both cgroup CPU quota and affinity: 2 CPUs on `0,1`, then 4 CPUs on
`0,1,2,3`. This models constrained CPU capacity within a VM; it does not establish
performance on an independent affordable Linux server. Platform comparisons use
relative results within each platform rather than comparing different native builds.

## Reversible control handoff

Seek and filter replacement now transfer exclusive machine ownership to a two-worker isolated
executor shared by the scheduler. Its queue is bounded by live admission capacity. A machine
cannot enter either CPU or control execution twice; wakes during the handoff are coalesced,
and returning always requeues it to observe pending cancellation or later controls. The same
seek ticket, generation checks, decoder/encoder state and acknowledgements are retained. A seek
can invalidate full output without waiting for its consumer. Filter replacement first preserves
already accepted PCM. Whitelisted CPU-only native filters may now start on scheduled owned-memory sessions.

Diagnostics report control workers, queued/current handoffs and cumulative handoffs separately
from permanent fallback counters. A temporary handoff is not reported as dedicated playback.
The production dedicated path remains unchanged. Crossfade continues on dedicated execution.
Deterministic regression coverage performs eight seeks and eight graph replacements while
paused, returns to scheduled playback, completes, and compares every Opus payload and sample
position to the dedicated backend. Existing stalled-consumer cancellation and lifecycle stress
also run through the bounded control executor.

## Bounded startup and local demux isolation

Eligible owned-memory and file sessions use two shared startup workers and a bounded admission
queue; overload returns an explicit worker-initialization failure. Unsupported workloads retain
their dedicated backend. A startup which discovers an unsupported demuxer, or exceeds live
scheduler capacity, transfers its machine to a tracked dedicated worker; stop joins both the
startup task and that fallback. No unsupported generation occupies a startup worker indefinitely.

Local files using the conservative demuxer whitelist now isolate the FFmpeg input context on
two shared demux workers. Codecs, resampling, packet metadata normalization and filtering stay
on CPU workers. Each read burst admits up to eight AVPackets, stopping at a 128 KiB watermark;
a single oversized native packet is preserved whole. A queue contains no more than eight
packets and crosses the watermark by at most one packet. A source has at most one read burst
in flight. Reads are requested only at the low watermark; full output eventually stops further
input demand. Empty packet queues return explicit WaitingInput and register the session wake.
Native EOF, codec drain, trimming metadata, Opus history/recovery and resampler drain retain the
same logic for both backends. No entire-file preload or new unsafe Send/Sync implementation is
used. The FFmpeg binding already marks its exclusively owned input and packets Send; cloned
codec parameters own independent native storage. Native contexts are never used concurrently.

Seek handoff waits for its own read burst, then seeks the original context and invalidates old
read-ahead only after success. A weak cleanup registration avoids retaining idle demux contexts;
stop joins outstanding reads outside CPU workers, and cancellation closes the native input
before publishing read completion. Tests compare complete PCM and Opus payloads across direct
and queued file demux with three seeks and filter replacement/removal, including trimmed and
passthrough Opus. Thirty-two simultaneously stalled consumers cancel and reclaim all queues.
A deterministic injected stall of both I/O workers leaves unrelated owned-memory startup and
playback able to complete. A full startup/isolated queue rejects work without growing threads.

HTTP/plugin eligibility is deliberately deferred. Bounded blocking I/O alone cannot guarantee
fairness when every worker is indefinitely stalled in a producer callback. FFmpeg demuxers do
not provide a general promise that arbitrary AVIO EAGAIN or interrupt-based suspension can be
resumed without losing parser state; changing those callbacks requires container-specific
regressions and readiness integration first. Local filesystem kernel stalls can still delay
other local-file reads; owned-memory codec workers remain independent. Crossfade stays dedicated.

The available-parallelism convenience constructor drops the old four-worker cap and uses an
eight-action budget. This follows the previous measured ten-worker synchronized-tail recovery
and dispatch-overhead results. Explicit constructors keep their previous budgets. FIFO and the
existing ordered deadline queue remain; no affinity, stealing or dynamic resizing is added.

An additional deterministic regression holds an actual routed Discord finish acknowledgement,
completes unrelated playback on the same single CPU worker, seeks the pending-EOF session, and
checks the resumed generation's sample continuity through natural EOF. Queued controls and
in-flight barriers now carry generation ownership; stale flush/pause/resume/finish operations
are rejected and stale barriers cannot finish the new timeline. Read-ahead registers a wake
only for an empty queue, so speculative input completion does not interrupt output pacing.

The blocking audit also found that one-input/one-output native graphs can perform I/O (for
example metadata printing) or invoke plugins. Native graph inspection therefore admits only
a conservative native filter set listed in the Fumadocs guide; the final filter-shape
audit below additionally requires unbuffered, rate-preserving output.
Initial unsupported graphs bypass bounded startup, using the cached classification from their
existing caller-side validation. Live unsupported graphs use a tracked dedicated fallback,
with separate control/filter fallback counters; pending encoder production drains before the
output returns to its synchronous contract. These workloads are never scheduler evidence.
The ordinary seek and whitelisted filter path still returns to CPU workers. This conservatism
preserves existing graph functionality without allowing hidden I/O onto codec workers.

Seek output invalidation is also resumable. Native decoder/filter repositioning returns from the
bounded isolated executor before polling the output flush. The machine retains the request and
acknowledges it only after transport invalidation and encoder alignment. Three detached Discord
seeks can remain pending while unrelated seek/filter controls succeed on the same one-worker
CPU pool, proving transport replacement does not exhaust the two isolated control workers.
A pause may supersede a pending EOF barrier; resume requests a fresh acknowledgement without
re-encoding or replaying tail packets. Additional byte comparisons cover draining retained PCM
before a deliberately unsupported graph returns the Opus output to synchronous delivery.

Shared startup additionally sets FFmpeg's format whitelist before read_header/find_stream_info;
post-probe classification alone would allow unsupported containers to open nested inputs while
occupying a shared startup worker. A failed restricted attempt retries the unchanged source on
a tracked dedicated worker and preserves the original readiness handshake and error behavior.
Diagnostics expose probe_retries; qualified benchmark rows require zero retries. A regression
opens owned WebM through this fallback, drains valid audio, joins both workers, and verifies
zero scheduled admissions and the explicit demuxer fallback reason.

## Seek correctness discovered during qualification

A four-seek PCM qualification run exposed read-history-dependent MP3 output in the
scheduled backend, including owned-memory sources. The experiment is rejected whenever
its within-source checksums differ; both partial series and the diagnostic reproducer
are retained under `data/hybrid/eligibility`, excluded from performance conclusions.

Native MP3 seek now requests one second of preroll to reconstruct the bit reservoir
and synthesis state, then trims at the requested sample. Actual landing can be earlier
according to the demuxer index. MP2/AAC use the same preroll for inter-packet state. FFmpeg seeks at or before the landing timestamp. Resampler
trimming accounts for stream start, buffered delay and cumulative output samples even
while discarding preroll. A feature-independent decoder regression compares every float
bit and the remaining sample count against continuous decoding after different prior
read counts, at 20, 70, 130, 1500 and 2700 ms. This correction is shared by both backends.
The control benchmark therefore compares corrected dedicated and scheduled execution;
original main remains in every ordinary playback comparison. Original seek output is
not treated as a golden reference. Preroll adds decode work in bounded codec turns after demux
repositioning and is included in total CPU cost. Control latency measures the command
acknowledgement, not the first delivered post-seek sample; those are distinct events.

## Reproducing the final suites

The baseline revision is `55b71e55b38722401dbd6d0011ccf31b0337000c`. Broad macOS
suites freeze product code at `7e489aa`, including shared seek corrections. `b763e7e`
adds the byte-headroom guard and the 44.1 kHz seek reference case; post-boundary macOS
and constrained-Linux suites use that revision. `158c7f3` adds the final filter admission
guard, which disqualifies none of these performance workloads. Other later commits contain
scripts, documentation and measurements. Original source is archived independently and receives
only the common benchmark harness. The disabled harness feature declaration does not
change original playback. Previous target directories and raw results remain intact.

On this macOS host:

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig
export RUSTFLAGS='-L native=/opt/homebrew/opt/lame/lib -L native=/opt/homebrew/opt/svt-av1/lib -L native=/opt/homebrew/opt/openssl@4/lib -l c++'
bash benchmarks/hybrid-ab-build.sh /tmp/remedia-hybrid-final
CARGO_TARGET_DIR=/tmp/remedia-hybrid-final/scheduled-target cargo test --release --locked -p remedia-discord --features experimental-scheduler --lib --no-run
/tmp/remedia-hybrid-final/scheduled-target/release/examples/resource_audit_fixtures /tmp/remedia-hybrid-tail/fixtures
python3 benchmarks/hybrid-eligibility-run.py --suite tuning --release-dir /tmp/remedia-hybrid-final --fixtures /tmp/remedia-hybrid-tail/fixtures --output /tmp/fresh-mac-tuning.jsonl
```

Repeat the last command with `--suite controls`, `mixed` or `sustained`, choosing a
fresh output path. For `transport`, also supply the executable path printed by the
Discord `--no-run` build as `--transport-binary`. Finish builds/checks before any
measurement. The runner executes one child at a time; variants rotate/reverse across
three repetitions. `hybrid_ab.py` exposes every count, worker, budget and workload
argument for smaller or extended runs. It refuses to overwrite results/manifests.

For Linux, `benchmarks/hybrid-linux-build.sh /tmp/remedia-hybrid-linux` creates the
container and native/release builds. `HYBRID_REUSE_NATIVE=1` reuses an existing audited
prefix; `HYBRID_LINUX_RELEASE_DIR` chooses a separate in-container release directory.
Copy the same generated fixtures into `/tmp/remedia-hybrid-linux/fixtures`, then:

```sh
docker update --cpus 2 --cpuset-cpus 0,1 remedia-hybrid-linux
docker exec remedia-hybrid-linux python3 /repo/benchmarks/hybrid-eligibility-run.py --suite linux --release-dir /experiment/releases --fixtures /experiment/fixtures --output /experiment/fresh-linux-2cpu.jsonl
docker update --cpus 4 --cpuset-cpus 0,1,2,3 remedia-hybrid-linux
# Repeat with a fresh 4cpu output filename. Stop the container after collecting results.
```

Use `/experiment/releases` unless a different directory was selected during build.
The checked-in final manifests preserve exact commands, fixture hashes and binary
hashes. Generate aggregate statistics with `hybrid-eligibility-summary.py`; it rejects
incomplete repetitions, failed audio qualification, fallback admissions and probe retries.

## Measurement interpretation

Child kernel CPU time includes startup, controls, drain and cleanup and excludes the
parent sampler. CPU per audio second divides by actual delivered samples, not requested
concurrency or nominal duration. Peak RSS/threads are sampled every 2 ms and may miss
shorter peaks; active snapshots also record physical footprint on macOS. Private/native
heap totals and allocation counts are not measured in this suite. Footprint includes
stacks, native codec state and allocator retention; it is not a heap attribution.

Queue contention counts failed immediate mutex acquisition and total measured wait.
Worker busy time sums elapsed action wall time, including preemption; its fraction of
worker capacity is not a CPU utilization counter. Startup percentiles measure each
synchronous `play` readiness handshake. Controls measure acknowledgements. Delivery
lateness/gaps use per-session media samples and delivered timestamps; they are continuity
proxies, not a physical audio-device underrun measurement. The start gate synchronizes
ordinary workloads; mixed sources additionally stagger admission by 1 ms.

The Discord performance harness exercises the actual sender/router admission and
barriers against a bounded asynchronous controlled consumer on two Tokio threads. It
omits network, RTP encryption and DAVE cost; its queue model is not the full production
actor's additional internal queue. Deterministic RTP/transport/DAVE loopback regressions
verify both encryption modes separately, including scheduled payloads. No live Discord
quality, packet-loss behavior or end-to-end sustainable capacity is claimed here.

## Final macOS file results

All 462 ordinary file runs passed sample, packet/chunk continuity and byte checksums
against original main and refactored dedicated execution. Every scheduled row admitted
exactly its requested session count, with zero capacity, input/output/demux/filter/control
fallbacks and zero probe retries. Counts cover 1, 8, 32, 64, 128, 256 and 512 sessions;
workers 2/4/10 and budgets 1/8/32, three interleaved repetitions. Files have independent
FFmpeg input/decoder ownership, not shared demux state or an entire-file memory preload.

Median child CPU milliseconds per delivered audio second, budget 8 / four workers:

| Sessions | MP3 original | MP3 dedicated | MP3 scheduled | Opus original | Opus dedicated | Opus scheduled |
| -------: | -----------: | ------------: | ------------: | ------------: | -------------: | -------------: |
|        1 |       10.370 |        10.371 |        10.383 |         5.479 |          5.383 |          5.423 |
|        8 |        7.260 |         7.348 |         6.037 |         1.486 |          1.418 |          1.306 |
|       32 |        6.982 |         6.980 |         5.178 |         0.940 |          0.961 |          0.774 |
|       64 |        7.013 |         7.003 |         5.118 |         0.805 |          0.804 |          0.677 |
|      128 |        6.938 |         6.980 |         5.031 |         0.756 |          0.742 |          0.623 |
|      256 |        6.991 |         6.981 |         5.110 |         0.699 |          0.717 |          0.592 |
|      512 |        6.972 |         6.966 |         5.405 |         0.702 |          0.708 |          0.589 |

At 512 sessions this is 22.4% less MP3 CPU and 16.8% less Opus CPU than corrected
dedicated execution. Original and refactored dedicated CPU are close in these ordinary
workloads. Extraction alone is not the source of the CPU savings.

Median resources at 512 sessions (threads are active / sampled peak / sampled startup peak;
context switches are voluntary / involuntary as reported by the platform):

| Workload / backend  | CPU ms/audio s | Threads         | Peak RSS MiB | Active footprint MiB | Wall s | Context switches |
| ------------------- | -------------: | --------------- | -----------: | -------------------: | -----: | ---------------: |
| MP3 original        |          6.972 | 513 / 513 / 510 |        243.9 |                115.1 |  1.239 |        1 / 14176 |
| MP3 dedicated       |          6.966 | 513 / 513 / 512 |        241.8 |                115.1 |  1.222 |        1 / 13726 |
| MP3 scheduled 4/8   |          5.405 | 11 / 11 / 11    |        198.3 |                100.7 |  1.922 |        0 / 46389 |
| MP3 scheduled 10/8  |          6.605 | 17 / 17 / 17    |        198.3 |                100.7 |  1.157 |        0 / 45297 |
| Opus original       |          0.702 | 513 / 513 / 508 |        170.5 |                 79.2 |  0.227 |         0 / 7149 |
| Opus dedicated      |          0.708 | 513 / 513 / 504 |        196.3 |                 79.2 |  0.235 |         1 / 8697 |
| Opus scheduled 4/8  |          0.589 | 11 / 11 / 11    |        170.2 |                 64.8 |  0.258 |         1 / 6106 |
| Opus scheduled 10/8 |          0.808 | 17 / 17 / 17    |        252.6 |                 64.9 |  0.228 |        1 / 21613 |

The eleven-thread file configuration is four CPU + two startup + two control + two
demux + caller. Thread reduction includes startup, not merely steady-state sampling.
At four workers, MP3 peak RSS falls 18.0% and active footprint falls 12.5%; Opus active
footprint falls 18.2%. Opus peak RSS at ten workers instead regresses. Snapshot footprint
and sampled peak RSS describe different phases; neither establishes a native heap cause.
Darwin and Linux classify context switches differently; compare each within its platform.

Four-worker CPU savings also reduce unpaced MP3 throughput: draining 1536 delivered
audio seconds takes 1.922 wall seconds versus 1.222 dedicated. Ten workers recover
throughput but save only 5.2% MP3 CPU and regress Opus CPU by 14.1%. Unpaced lower CPU
cost is therefore not sufficient evidence for choosing a realtime worker count.

### One action per turn

At 512 Opus file sessions / four workers, budget 1 uses 0.730 CPU ms/audio s versus
0.589 with budget 8 (19.3% reduction). Median turns fall from 80896 to 11278, queue
mutex wait falls from 127.3 to 4.3 ms, and peak RSS falls from 289.9 to 170.2 MiB.
With ten workers, budgets 1 and 8 cost 1.130 and 0.808 respectively (28.5% reduction),
with queue wait falling from 581.4 to 104.3 ms. One-action turns impose excessive
synchronization cost on this light packet workload. For four-worker MP3 transcoding,
1 versus 8 changes CPU only 5.426 to 5.405: native encoding dominates that comparison.

Budget 32 can improve passthrough CPU/RSS further, but increases the fairness quantum;
it was not tested in the final paced suites. Budget 8 is the conservative opt-in
convenience policy, not a universally optimal setting. These observations justify
simple bounded bursts, not work stealing, affinity or a new deadline-priority policy.
The existing ordered pacing deadlines remain; dynamic resizing is deferred.

### Repeated controls

All 27 corrected dedicated/scheduled control rows pass. Each session pauses, performs
four native seeks and four alternating volume-filter replacements, resumes and delivers
the complete remaining PCM from the final target. Within-source and cross-backend PCM
checksums match. At 256 sessions, median CPU costs are 1.396 dedicated, 1.482 scheduled
4/8 and 1.527 scheduled 10/8 ms/audio s: regressions of 6.1% and 9.3%. Seek acknowledgement
p99 is 0.024, 0.041 and 0.042 ms respectively in this warm local-file setup. These are
command acknowledgements, not first post-seek audio latency. All scheduled rows retain
256 admissions and zero fallbacks; temporary handoffs do not count as dedicated playback.

### Mixed sources and sustained pacing

All 48 mixed-source rows pass with independently opened MP3, Ogg Opus and ADTS AAC
sources, staggered startup, PCM and Opus outputs, and 32/128 sessions. At 128 sessions,
Opus-output CPU is 5.186 original, 5.181 dedicated, 3.930 scheduled 4/8 and 5.189
scheduled 10/8 ms/audio s. PCM costs 1.806, 1.809, 1.836 and 1.858 respectively:
scheduled PCM slightly regresses despite Opus-output savings. Peak RSS for Opus is
97.2 / 93.5 / 84.9 / 85.1 MiB. Mixed-codec equivalence is checked per source rather
than requiring different codecs to produce the same bytes.

Nine 128-session, 15-second realtime Opus-file runs pass with identical output. Median
CPU is 2.095 original, 2.094 dedicated and 1.112 scheduled 10/8 ms/audio s (46.9% below
dedicated); peak RSS is 62.2 / 62.5 / 58.1 MiB. Wall times are 15.055 / 15.047 /
15.054 seconds. P99 of per-session worst delivery lateness is 31.44 / 30.71 / 33.54 ms;
p99 of worst excess inter-chunk gap is 28.79 / 30.26 / 32.54 ms. The small timing
regression is retained rather than hidden behind CPU savings. These are three short
sustained repetitions, not hours-long stability or a hard realtime quality guarantee.

## Unsupported workloads and operational limits

HTTP(S), plugin/producer streams, pipes/devices, unsupported containers, synchronous
custom outputs and crossfade remain dedicated. Non-whitelisted native filters retain
all existing functionality on dedicated execution; live replacement permanently falls
back only for those unsafe graphs. No fallback workload contributes to performance
claims. The separate stalled producer diagnostic records 16 input fallbacks, zero
admissions and 43 total OS threads, including 16 benchmark caller-driver threads.
The diagnostic is explicitly marked `scheduledPerformanceEvidence: false`.

Bounded queues prevent memory/thread growth within scheduled admission; unsupported
fallbacks and capacity fallback are still dedicated and do not bound total engine
thread count. A pool contains six fixed isolation threads in addition to CPU workers,
including when some executors are idle. This fixed overhead can be unfavorable for
small workloads. Native codec/filter actions have a bounded action count, not a hard
wall-time guarantee. All compatibility/cancellation contracts remain in place.

Local regular files assume a responsive filesystem. A kernel-blocked filesystem read
can occupy a demux worker until the OS returns; blocking open/probe can occupy startup
workers. Exhausting those workers delays other operations needing the same executor,
although established owned-memory codec playback does not need them. Stop joins those
operations; it cannot preempt an uninterruptible kernel call. This is one reason HTTP /
plugin scheduling is deferred rather than represented as a safe bounded-thread solution.
The existing fallback-seek reopen path can still use temporary native preparation threads;
ordinary local native seeks are the path qualified by the final control benchmark.

No worker affinity, work stealing, deadline-priority dispatch or dynamic resizing is
introduced. The evidence isolates dispatch overhead and burst capacity; it does not
show FIFO ordering as the cause of timing regressions strongly enough to justify a
more complex policy. Actual packet-loss concealment, physical-device underruns, native
allocation attribution and hours-long mixed lifecycle stability remain unmeasured.

### Oversized packet audit follow-up

A final queue-bound review found a low-watermark edge case: consuming a small packet
in front of an oversized queued packet could request another read although the byte
watermark was already exceeded. Read-ahead now requires both packet and byte headroom.
A deterministic test occupies the demux worker, injects a small/oversized packet pair,
and verifies no read is submitted until the oversized packet is consumed. Packets
remain whole and the existing single-packet overshoot allowance is preserved.

The broad macOS tuning, mixed, sustained and transport suites were frozen at `7e489aa`.
A separate post-fix release comparison and the constrained-Linux suites qualify the
additional byte-headroom check. Their manifests identify the distinct binaries. The
pre-fix rows remain available; no previous artifact or result is overwritten.

## Discord queue-boundary results

All 162 controlled sender/router runs pass: 8/128/512 sessions, MP3 and Opus owned
sources, paced/scheduled-offset/consumer-stall cases, dedicated versus 4/8 and 10/8,
three repetitions. Every scheduled run admits exactly the requested sessions with zero
fallbacks. The 100 ms stall is injected into every eighth consumer after packet 25;
its expected gap is retained. All delivered payload checksums, sample counts and packet
positions match dedicated execution; finish acknowledgements complete successfully.

At 512 sessions (p99 here is the percentile of per-session worst observations across
three repetitions, not a percentile of all packets):

| Source / case / backend    | CPU ms/audio s | Peak RSS MiB | Peak / startup threads | Lateness p99 ms | Excess gap p99 ms |
| -------------------------- | -------------: | -----------: | ---------------------- | --------------: | ----------------: |
| MP3 paced dedicated        |          7.246 |        269.8 | 516 / 507              |           10.86 |              7.81 |
| MP3 paced scheduled 4/8    |          4.760 |        231.5 | 14 / 14                |           12.64 |             10.79 |
| MP3 paced scheduled 10/8   |          7.042 |        231.8 | 20 / 20                |           13.15 |             11.96 |
| MP3 offset dedicated       |          7.451 |        269.5 | 516 / 511              |           15.93 |             14.64 |
| MP3 offset scheduled 4/8   |          4.898 |        231.0 | 14 / 14                |           12.09 |             10.74 |
| MP3 offset scheduled 10/8  |          7.332 |        231.4 | 20 / 20                |            6.98 |              4.72 |
| Opus paced dedicated       |          1.932 |        362.7 | 516 / 510              |           50.83 |             40.60 |
| Opus paced scheduled 4/8   |          1.368 |        316.0 | 14 / 14                |           71.93 |             69.72 |
| Opus paced scheduled 10/8  |          1.797 |        316.7 | 20 / 20                |           17.57 |             16.12 |
| Opus offset dedicated      |          2.041 |        362.8 | 516 / 514              |           28.97 |             25.26 |
| Opus offset scheduled 4/8  |          1.353 |        314.5 | 14 / 14                |           16.86 |             14.22 |
| Opus offset scheduled 10/8 |          1.911 |        314.2 | 20 / 20                |           11.07 |              9.73 |

The consumer offset case delays each initial consumer by `(index % 10) * 10 ms`; it
spreads packet demand, unlike the file suite's staggered admissions. The Opus tail
transition remains a synchronized codec burst: four workers reduce CPU but worsen paced
lateness, whereas ten workers reduce CPU by only 7.0% yet improve the tail timing in
this harness. Using all available CPU parallelism is justified as a conservative burst
policy, not as the cheapest steady-state pool. It still requires workload-specific tuning.

In stall cases, p99 lateness on **un-stalled** sessions at 512 is MP3 20.21 / 11.26 /
8.53 ms and Opus 32.88 / 70.31 / 17.04 ms for dedicated / 4/8 / 10/8. A full queue
parks its producer rather than holding a CPU worker; the four-worker Opus burst remains
visible on unrelated sessions and is not excused as consumer backpressure. At four
workers / MP3 paced, involuntary switches fall from 98066 to 19237; the Opus paced
comparison falls from 90698 to 21015, but ten-worker Opus still records 84553. These
measurements do not imply equivalent savings in the full encrypted network actor.

### Post-boundary current-source comparison

The separate 48-row `final-mac-post-boundary.jsonl` series repeats 128/512-session
MP3 and Opus files against original main, current dedicated and current scheduled
4/8 and 10/8. All rows pass identical delivery; scheduled admissions equal requested
counts and every fallback/probe-retry counter is zero. At 512 sessions:

| Source | Original CPU | Dedicated CPU | Scheduled 4/8 CPU | Scheduled 10/8 CPU | Dedicated / 4/8 / 10/8 peak RSS MiB |
| ------ | -----------: | ------------: | ----------------: | -----------------: | ----------------------------------- |
| MP3    |        6.809 |         6.852 |             5.022 |              6.437 | 238.0 / 197.7 / 198.8               |
| Opus   |        0.702 |         0.686 |             0.581 |              0.833 | 150.2 / 165.9 / 257.3               |

CPU units are ms per delivered audio second. Current four-worker CPU reductions are
26.7% MP3 and 15.3% Opus versus current dedicated. The extra bounds check is not claimed
as the cause of differences from the broader sweep; these are separate interleaved
series on a shared host. Opus RSS changes substantially across series, including a
current 10.5% regression at four workers. Peak RSS savings are therefore not established
for passthrough. The consistent active-footprint reduction is a separate observation.

## Final filter admission audit

The processor's existing `write_frame` / EOF drain can pull several native filter
frames synchronously. That is incompatible with a scheduling output retaining one PCM
frame: a second Opus frame would fail admission, while PCM stream backpressure could
block a codec worker. A new deterministic native regression demonstrates multiple
output frames from one 2400-sample input using buffered lowpass EQ. CPU-only classification
alone was therefore insufficient.

Eligibility now also requires 48 kHz on every configured link, unbuffered EQ/biquads
(`blocksize=0`), and only automatically inserted format-conversion resamplers. `atempo`,
explicit resampling/rate changes and buffered EQ retain dedicated execution, just like
I/O/plugin filters. Normal volume, pan and unbuffered EQ remain eligible and reversible.
Initial and live unsupported graphs preserve their native dedicated implementation;
fallbacks are explicit and never scheduler evidence. New PCM/Opus byte comparisons cover
tempo, resampling and buffered-EQ fallback while unrelated one-worker scheduled playback
continues. No queue is enlarged and no filtered frame is discarded to claim support.

A genuinely resumable filter-output drain is still required before admitting variable-output
graphs. It is deferred rather than adding an unbounded intermediate PCM queue. The frozen
performance suites contain no newly ineligible graphs; control suites use admitted volume
only. Their source revisions remain recorded, and final feature-on/off checks cover this
additional admission guard.

## Constrained Linux results

Both 96-row suites pass complete delivered-audio equality across original, refactored
dedicated, scheduled 2/8 and scheduled 4/8. Counts are 8/64/256/512, with two source
codecs and three interleaved repetitions. Scheduled admissions always equal requested
sessions, with zero fallback/retry counts. All these workloads remain eligible after
the final filter admission guard. Native libraries and encoder quality are identical
within each Linux comparison; native build/link choices differ from macOS, so absolute
CPU rates are not compared between platforms.

Median results at 512 sessions, including overloaded cases:

| Quota / source / backend | CPU ms/audio s | Peak RSS MiB | Active private MiB | Peak / startup threads | Lateness p99 ms | Excess gap p99 ms | Wall s |
| ------------------------ | -------------: | -----------: | -----------------: | ---------------------- | --------------: | ----------------: | -----: |
| 2 / MP3 original         |          5.664 |        212.1 |               98.6 | 513 / 511              |          4108.7 |            3962.0 |   4.58 |
| 2 / MP3 dedicated        |          5.231 |        218.9 |              102.5 | 513 / 511              |          3673.0 |            3404.4 |   4.25 |
| 2 / MP3 scheduled 2/8    |          5.510 |        162.3 |               86.4 | 9 / 9                  |          1241.8 |             128.7 |   4.37 |
| 2 / MP3 scheduled 4/8    |          5.308 |        164.9 |               88.3 | 11 / 11                |          1094.4 |             135.2 |   4.21 |
| 2 / Opus original        |          1.231 |        233.2 |               91.5 | 513 / 511              |           304.9 |             307.2 |   3.49 |
| 2 / Opus dedicated       |          1.225 |        232.7 |               91.5 | 513 / 512              |           296.2 |             299.0 |   3.48 |
| 2 / Opus scheduled 2/8   |          0.942 |        102.3 |               65.6 | 9 / 9                  |           251.3 |             246.1 |   3.43 |
| 2 / Opus scheduled 4/8   |          0.983 |        103.1 |               65.6 | 11 / 11                |           229.4 |             227.4 |   3.42 |
| 4 / MP3 original         |          5.969 |        198.0 |               98.6 | 513 / 511              |            47.7 |              53.1 |   3.21 |
| 4 / MP3 dedicated        |          5.617 |        200.0 |              100.7 | 513 / 511              |            23.9 |              23.6 |   3.22 |
| 4 / MP3 scheduled 2/8    |          5.909 |        162.5 |               86.4 | 9 / 9                  |          1138.4 |             129.1 |   4.27 |
| 4 / MP3 scheduled 4/8    |          5.569 |        165.2 |               88.3 | 11 / 11                |            12.5 |              23.3 |   3.19 |
| 4 / Opus original        |          1.407 |        241.9 |               93.5 | 513 / 511              |           170.8 |             169.7 |   3.38 |
| 4 / Opus dedicated       |          1.435 |        237.7 |               93.5 | 513 / 513              |           157.0 |             168.3 |   3.37 |
| 4 / Opus scheduled 2/8   |          0.952 |         99.9 |               63.6 | 9 / 9                  |           236.1 |             236.0 |   3.43 |
| 4 / Opus scheduled 4/8   |          0.974 |        101.5 |               63.6 | 11 / 11                |           130.3 |             130.2 |   3.31 |

At four CPUs / four workers, MP3 CPU improves only 0.9% versus refactored dedicated;
this misses the CPU target, although peak RSS falls 17.4%, private memory falls 12.3%
and lateness improves. Opus CPU falls 32.1%, peak RSS 57.3% and private memory 32.0%.
The Opus private-memory reduction is about 60 KiB per session at this count; peak RSS
is a different, later phase. This is evidence of material memory savings, not merely
fewer thread IDs.

Two workers on four CPUs leave MP3 codec capacity unused: 4.27 seconds to deliver each
three-second source, and 1.14 seconds p99 lateness. All 512-session MP3 configurations
on two CPUs are overloaded. Completion and correct samples do not establish sustainable
realtime concurrency. These overloaded rows stay in the results; no buffering is relaxed
to make them complete faster. Even four-worker Opus has a 130 ms tail-gap percentile at
512, so this fixture does not establish strict audio-deadline continuity.

Regressions also appear below saturation. At 64 sessions on two CPUs, MP3 CPU is 6.231
dedicated versus 8.183 / 8.243 scheduled (31.3% / 32.3% higher); p99 lateness is 1.6
versus 13.8 / 9.6 ms. At four CPUs / 64 Opus sessions, two workers save CPU (3.044 to
1.893) but worsen lateness (41.9 to 62.3 ms); four workers cost 2.282 with 40.2 ms
lateness. Fixed pool sizing is a real tradeoff, not a universal CPU win.

At four CPUs / 512 MP3 sessions, voluntary/involuntary switches are 152813 / 14751
dedicated versus 69238 / 24054 scheduled 4/8. Queue mutex wait is 51.8 ms over the run;
Opus queue wait is 57.6 ms, with switches 185366 / 2749 versus 86915 / 2008. On two
CPUs / 512 MP3, scheduling two workers instead increases switches to 94978 / 92734
from 14216 / 7708 dedicated, despite only 0.1 ms measured queue-lock wait. Queue-lock
waiting alone does not explain scheduler cost. Wake-ups, pacing, demux handoffs and
native codec/cache behavior need further profiling before another policy is justified.

Startup handshake p99 at four CPUs / 512 is MP3 0.36 ms dedicated / 0.31 ms scheduled
4/8, and Opus 1.15 / 0.42 ms. The 2 ms thread sampler can miss shorter startup spikes;
active and total peak thread counts are reported separately in raw rows. Worker busy
fractions, queue peaks and all concurrency levels are in `summary-final.json`.

## Verification and decision

The final dataset contains 948 qualified child runs, all with complete audio delivery
and explicit admissions. Failed controls, diagnostic-only fallbacks and earlier tuning
rows are preserved separately and excluded from those totals. `summary-final.json`
contains three-run medians, pooled startup/control percentiles, queue contention,
worker busy wall fractions, memory and context switches. The raw rows retain every
source checksum, sample count, packet/chunk count and per-session timing observation.

The implementation demonstrates useful CPU and memory improvements for selected real
file and transport workloads. One-action dispatch has avoidable synchronization cost;
bounded bursts are justified. A pool sized only for average passthrough CPU is insufficient
for synchronized codec transitions. Increasing workers can recover timing while erasing
CPU savings. Controls, PCM, some Linux configurations and peak RSS show real regressions.

**Adopting the scheduler as the production default is not justified yet.** It remains a
non-default experiment with explicit fixed tuning and fallbacks. The next prerequisites
are a resumable filter drain, genuinely readiness-driven network/plugin demux, longer
resource-stability runs, first-post-seek audio latency, broader independent media, and
full encrypted/live transport qualification. Those prerequisites are not represented as
implemented or benchmarked. Crossfade remains deferred. Dedicated Node/RPC constructors
and production executor selection are unchanged.

Final local verification after the filter admission guard:

- `cargo fmt --all -- --check`, `cargo check --workspace`, Clippy workspace/all targets
  with `-D warnings`, and workspace tests with the feature both disabled and enabled pass.
  Rust totals are 518 / 545 passed; existing ignored benchmark tests remain explicit.
- N-API build, TypeScript build and binding tests pass in both feature configurations:
  108 tests across 16 files each. These are local Homebrew-linked debug bindings;
  shipping static-normalization is not claimed for that build.
- `pnpm lint`, `pnpm format:check`, `pnpm typecheck` and the supported docs build pass.
  The static site checks 60988 local links across 395 exported pages.
- Linux release tests pass the oversized-packet regression, final filter admission
  regression and all 14 scheduler integration tests. All three measured Linux executables
  pass the static media/TLS dependency audit. Full Linux workspace/Node suites were not run.
- Every final row passes exact known fixture sample counts, cross-repetition checksums,
  cross-backend chunks/packets, successful controls and explicit admission checks. CPU
  quota/affinity is reflected as 2/4 available CPUs in every Linux child snapshot.
- Measured executable hashes remain consistent with the manifests. Original binaries,
  earlier raw data and rejected diagnostic runs are retained. Docker is stopped after
  collection; its native/source/build artifacts remain available for reproduction.

[Raw data and inventory](data/hybrid/eligibility/README.md),
[aggregate statistics](data/hybrid/eligibility/summary-final.json),
[delivery qualification](data/hybrid/eligibility/qualification-final.json),
[interactive comparison](data/hybrid/eligibility/comparison.html), and
[check scope](data/hybrid/eligibility/validation-final.json) accompany this report.
The user guide is `apps/docs/content/docs/core/playback.mdx` under “Experimental shared executor”.
