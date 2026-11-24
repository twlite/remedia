# Hybrid scheduler: isolated comparison and processing budgets

This continuation measures the original `55b71e5` playback implementation, the refactored
feature-disabled dedicated backend, and the current experimental pool. Production selection stays
unchanged. Source milestone: `ea8691e` (budget/counter implementation: `ba1c68d`).

[Comparison charts](hybrid-ab-visual.html) summarize selected measured tradeoffs; the complete
matrices and resource medians remain the evidence. Regenerate with `python3 benchmarks/hybrid-ab-visual.py`.

## Method and reproducibility

Hardware remains Apple M5, 10 logical CPUs, 24 GiB RAM, macOS 27.0.1 arm64, Rust 1.99.1 and
Homebrew FFmpeg 9. Native dependencies, Opus settings, release profile and fat LTO are identical.
The original engine is extracted with `git archive`; only the common benchmark example and an
unused manifest feature declaration are added. No original playback implementation is edited.
The refactored dedicated executable is built with the Cargo feature disabled. The scheduled
executable enables the feature and explicitly creates a pool. All use the same harness source.
Previous preserved baseline binaries remain untouched.

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig
export RUSTFLAGS='-L native=/opt/homebrew/opt/lame/lib -L native=/opt/homebrew/opt/svt-av1/lib -L native=/opt/homebrew/opt/openssl@4/lib -l c++'
benchmarks/hybrid-ab-build.sh /tmp/remedia-hybrid-ab
/tmp/remedia-hybrid-ab/scheduled-target/release/examples/resource_audit_fixtures /tmp/remedia-hybrid-ab/fixtures
python3 benchmarks/hybrid_ab.py \
  --original /tmp/remedia-hybrid-ab/original-target/release/examples/hybrid_scheduler_benchmark \
  --dedicated /tmp/remedia-hybrid-ab/dedicated-target/release/examples/hybrid_scheduler_benchmark \
  --scheduled /tmp/remedia-hybrid-ab/scheduled-target/release/examples/hybrid_scheduler_benchmark \
  --fixtures /tmp/remedia-hybrid-ab/fixtures/music15.opus,/tmp/remedia-hybrid-ab/fixtures/music3.mp3,/tmp/remedia-hybrid-ab/fixtures/music3.aac \
  --output benchmarks/data/hybrid/ab/opus-matrix.jsonl
```

The main matrix covers 1, 8, 32, 64, 128, 256 and 512 sessions, three repetitions, 1/2/4 CPU
workers, and budgets of 1/8/32 actions per queue turn. A separate MP3-to-f32le matrix covers
32/128/512 sessions with the same variants/repetitions. Native fixture generation uses unchanged
128 kbps settings and does not spawn media-processing subprocesses. Every source has independent
owned storage per session. Default output bitrate, VBR and Opus complexity are unchanged.
Builds, Rust/Node tests, Clippy and docs generation finish before measurements begin. Repetitions
rotate and reverse variant order within each fixture/format/concurrency case.

A benchmark-only output gate holds production until every session has opened. Scheduled runs
also wait for all admissions or explicit fallbacks to be accounted for, with a ten-second startup
limit. The common gate forwards output methods and encoded capability; it neither changes audio
nor increases native stream queue limits. A dedicated worker sleeps at the gate; a CPU worker
registers a readiness waker and suspends. All scheduled rows must have the full requested count
in `activeScheduled`, exactly that many cumulative admissions and zero fallbacks, otherwise the
runner rejects the result. This avoids reporting fallback or sequential short-track completion
as pool concurrency. The gate does synchronize startup and adds wake-up work: this is a controlled
concurrent processing comparison, not phased realtime traffic or a production startup trace.

`ungated-pilot.jsonl` is a stopped, incomplete pilot retained separately. Three-second encoded
output can finish below the 64 KiB readiness boundary while sessions open sequentially. Its
nominal concurrency therefore does not prove live concurrency. It is excluded from conclusions.

The parent samples child RSS and OS threads every approximately 2 ms and separates the startup
window using the child's timestamp and startup duration. These are sampled peaks and can miss
shorter transients. Child kernel user/system CPU and context switches exclude parent sampler CPU.
In-process snapshots record physical footprint, lifetime peak RSS and live resources immediately
before gate release and after stop. Memory includes input copies, native codecs, queue payloads,
stacks and allocator retention; footprint is not a native heap inventory.

Every run asserts packet/sample-position continuity, successful completion and stop, positive
sample counts, equal per-session work and equal payload checksums. Every paired variant additionally
matches the complete sample, chunk and checksum arrays against the original result. Tests compare
stream payloads byte for byte. No fallback row qualifies as scheduler performance evidence.

## What the processing budget measures

`new(workers, capacity)` retains a one-action turn. `with_processing_budget` configures a positive
maximum action count per turn; it does not bound native action wall time. Output readiness and
controls are checked on each action, and suspension/completion/migration ends a burst immediately.
Each burst still owns exactly one job; wake coalescing and single-session exclusion are unchanged.
A larger budget saves FIFO mutex operations and dispatches at the cost of a larger fairness quantum.

Diagnostics record cumulative admissions, currently live scheduled sessions (including suspended
ones), control/capacity/unsupported fallbacks and unsupported input/output/demux/filter/crossfade
reasons. Queue depth peaks, action counts, dispatch counts, contended enqueue/dequeue acquisitions
and aggregate wait nanoseconds are measured. Mutex wait includes descheduling and excludes
condition-variable reacquisitions; it is not isolated lock instruction time. CPU includes these
experimental diagnostics. The measurement does not separate cache-coherence cost, machine-state
mutexes, playback-state locks and native codec work into an allocation or hardware-counter profile.

## Complete unpaced results

All 792 gated runs completed and matched the original delivered audio. Every scheduled row had
the full requested live scheduled count at release, cumulative admissions equal to that count,
and zero input/output/demux/filter/crossfade/capacity/control fallbacks. There are 693 Opus-output
rows plus 99 PCM-output rows. Full medians/ranges and resource counters are in `ab/summary.json`.

The following are three-run medians at 512 genuinely live sessions. CPU uses the child kernel
user+system delta divided by the exact delivered audio duration, including process startup,
gating, consumption/checksum work and cleanup. Smaller is better; these are not isolated codec costs.

| Backend   | Workers | Budget | Opus passthrough CPU ms/audio s | MP3→Opus | AAC→Opus | MP3→f32le |
| --------- | ------: | -----: | ------------------------------: | -------: | -------: | --------: |
| original  |       0 |      1 |                           0.447 |    6.899 |    7.254 |     1.410 |
| dedicated |       0 |      1 |                           0.457 |    6.934 |    7.261 |     1.378 |
| scheduled |       1 |      1 |                           0.159 |    4.977 |    5.294 |     1.079 |
| scheduled |       1 |      8 |                           0.146 |    5.002 |    5.297 |     1.069 |
| scheduled |       1 |     32 |                           0.144 |    5.149 |    5.452 |     1.061 |
| scheduled |       2 |      1 |                           0.224 |    5.048 |    5.347 |     1.296 |
| scheduled |       2 |      8 |                           0.180 |    5.042 |    5.322 |     1.253 |
| scheduled |       2 |     32 |                           0.179 |    5.277 |    5.580 |     1.262 |
| scheduled |       4 |      1 |                           0.288 |    5.284 |    5.394 |     1.528 |
| scheduled |       4 |      8 |                           0.262 |    4.995 |    5.368 |     1.303 |
| scheduled |       4 |     32 |                           0.265 |    5.294 |    5.607 |     1.306 |

Resource and timing examples at the same concurrency follow. Thread peaks are sampled total OS
threads; the live snapshot is separate. Startup peaks exclude the post-gate interval but may miss
sub-2ms transients. Context switches are raw kernel voluntary+involuntary counters; near-zero
voluntary counts on this Darwin collector do not mean no thread ever blocked.

| Workload          | Backend/workers/budget | Wall s | Live/peak/startup-peak threads | Peak RSS MiB | Active footprint MiB | Context switches | Queue wait ms |
| ----------------- | ---------------------- | -----: | ------------------------------ | -----------: | -------------------: | ---------------: | ------------: |
| music15.opus→opus | original/0/1           |  0.949 | 513/513/505                    |        391.1 |                252.5 |           323789 |          0.00 |
| music15.opus→opus | dedicated/0/1          |  0.960 | 513/513/509                    |        386.8 |                252.5 |           325955 |          0.00 |
| music15.opus→opus | scheduled/4/1          |  0.579 | 6/6/6                          |        518.5 |                244.8 |            52908 |        264.45 |
| music15.opus→opus | scheduled/4/8          |  0.554 | 6/6/6                          |        403.5 |                244.8 |            72322 |         74.88 |
| music3.mp3→opus   | original/0/1           |  1.180 | 513/513/506                    |        266.3 |                139.2 |             9131 |          0.00 |
| music3.mp3→opus   | dedicated/0/1          |  1.202 | 513/513/505                    |        265.4 |                139.2 |            10085 |          0.00 |
| music3.mp3→opus   | scheduled/4/1          |  1.890 | 6/7/7                          |        234.9 |                131.5 |            40650 |          9.05 |
| music3.mp3→opus   | scheduled/4/8          |  1.845 | 6/7/7                          |        226.1 |                131.5 |            29637 |          0.46 |
| music3.aac→opus   | original/0/1           |  1.304 | 513/513/509                    |        584.1 |                458.4 |            10448 |          0.00 |
| music3.aac→opus   | dedicated/0/1          |  1.328 | 513/513/507                    |        592.2 |                458.4 |            12099 |          0.00 |
| music3.aac→opus   | scheduled/4/1          |  2.057 | 6/7/7                          |        553.6 |                450.7 |            34508 |         13.15 |
| music3.aac→opus   | scheduled/4/8          |  2.029 | 6/7/7                          |        549.7 |                450.7 |            35872 |          0.41 |
| music3.mp3→f32le  | original/0/1           |  0.955 | 513/513/499                    |        254.1 |                139.1 |            68273 |          0.00 |
| music3.mp3→f32le  | dedicated/0/1          |  0.938 | 513/513/513                    |        254.2 |                139.2 |            65956 |          0.00 |
| music3.mp3→f32le  | scheduled/4/1          |  0.909 | 6/6/6                          |        253.5 |                131.6 |           189453 |         15.02 |
| music3.mp3→f32le  | scheduled/4/8          |  0.940 | 6/7/7                          |        245.6 |                131.6 |            62754 |          0.29 |

At 512 MP3→f32le sessions, four workers with one action per turn regressed CPU by 10.9% versus
the refactored dedicated driver. Budget eight improved it by 5.4%; budget 32 was similar. The
one-action policy therefore introduces material avoidable dispatch/synchronization overhead in
this workload. MP3→Opus improved from 5.284 to 4.995 ms/audio s when moving from budget one to
eight, but worsened to 5.294 at budget 32. AAC shows the same lack of monotonic improvement.
A larger burst is not automatically better and does not guarantee fairness latency.

The extraction itself is close to the original at these workloads: at 512 streams, dedicated
CPU changes are about +2.2% for passthrough, +0.5% for MP3 encoding, +0.1% for AAC encoding and
-2.2% for PCM output. These small differences are not a universal extraction-overhead guarantee.

The shared pool saves substantial CPU for encoded output on this host, but trades CPU against
throughput for transcoding. Four-worker/budget-eight MP3→Opus takes 1.845 s versus 1.202 s for
dedicated; AAC takes 2.029 s versus 1.328 s. One-worker configurations are slower still. Conversely,
passthrough finishes faster with a small pool. The raw runs must be evaluated against required
real-time audio demand rather than selecting the best CPU-only column.

Memory does not universally improve. Four-worker, one-action passthrough peaks at 518.5 MiB
versus 386.8 MiB for refactored dedicated, despite far fewer OS threads. Larger budgets reduce
this regression, while transcoding and PCM cases have different footprints. Input allocations,
queue occupancy and native allocator behavior are included; no native heap explanation is proven.

Queue wait is diagnostic evidence, not a complete causal CPU profile. For MP3→Opus at 512 streams
and four workers it drops from about 9.05 ms to 0.46 ms at budget eight, while CPU savings are
larger than that waiting time alone explains. Fast-path mutex/atomic cost, wake-ups, cache effects,
reader scheduling and host CPU-frequency differences remain involved. The heterogeneous Apple
CPU was not affinity-pinned. These results do not establish Linux vCPU behavior.

The evidence justifies testing pacing on the already readiness-aware stream output. It does not
justify marking blocking file/network input or Discord acknowledgements nonblocking, nor changing
the production backend or silently selecting a larger default budget.

## Eligibility investigation

The initial measurement gate preceded expansion of eligibility. It justified adding realtime
stream deadlines for owned bytes in the five whitelisted demuxers, without native filtergraphs.
Local files, network/plugin input, native filtergraphs and Discord/crossfade remain dedicated.
The following blocking boundaries remain explicit:

- Local files: FFmpeg demux reads and seeks remain synchronous. Loading an entire file into owned
  bytes would change retained-memory and input ownership guarantees. A shared bounded I/O pool
  could run packet-reader bursts into byte/packet-bounded queues, then wake codec workers, but
  filesystem stalls must not monopolize CPU workers or create a permanent thread per session.
  `AudioDecoder` currently owns its packet reader, codec, resampler and pending frames together;
  this separation requires an explicit ownership/seek-generation protocol, not a thread-pool wrapper.
- Realtime streams: the initial bridge discarded the absolute deadline, so merely enabling
  realtime there would have run unpaced. The continuation now adds an optional
  `scheduler_poll_deadline` capability and an ordered, capacity-bounded deadline queue shared by
  CPU workers. It has no additional timer thread and no per-session sleep/poll task. A control or
  cancellation wake removes the job's sole timer before making it runnable. Expiration appends
  due jobs to the FIFO; it does not guarantee CPU service at that instant under overload.
- Discord: `connection/sender.rs` packet submission uses a bounded channel with retrying 25 ms
  sends; control barriers wait for actor acknowledgements. `sender_router.rs` can wait for a new
  transport. Those methods must expose readiness and retained-packet ownership before opting in.
  Queue acceptance must remain distinct from RTP dispatch/played position, with existing generation
  checks, retry ownership, DAVE and encryption intact. No live Discord qualification is inferred
  from raw stream checksums or existing mock-transport tests.
- Seek/filter replacement: current controls permanently transfer the machine to a dedicated
  fallback. Keeping the CPU job requires control-operation outcomes/futures, acknowledged output
  transitions and generation-bound completion. A bounded control pool could temporarily own the
  machine and return it after completion, but must handle shutdown, cancelled seeks and filter
  initialization that may perform blocking work. It must not wait for acknowledgement on a CPU
  worker, publish completion before ownership returns, or acknowledge an unapplied transition.
- HTTP/plugin sources: custom AVIO invokes a synchronous read. `ByteStreamReader` waits on a
  condition variable; an asynchronous plugin producer does not make this consumer nonblocking.
  Returning EAGAIN speculatively needs demuxer-specific recovery tests, including partial headers,
  probing, errors and cancellation. A shared blocking pool alone still permits stalled sources
  to occupy every I/O slot. No unbounded encoded-byte or packet staging queue is acceptable.
- Crossfade: staging currently has a producer thread, bounded PCM cushion and decoder handoff.
  Promotion, passthrough recovery and fallback seeks share ownership-sensitive state. Merely
  scheduling the active worker leaves staging threads and blocking promotion intact. A future
  staged-source job needs the same readiness/generation protocol as an ordinary source and must
  preserve the audible tail rather than cutting it off to satisfy a scheduling budget.

These are source-audited design constraints, not implemented support or performance evidence.

## Pacing implementation and qualification

After the initial gate, `702ff29` added generation-specific deadlines to the existing resumable
machine and FIFO. The readiness mutex also owns the ordered timer map. A job is either runnable,
running, suspended, timed, or done. Wake-up atomics remove a timed entry before enqueueing;
notification during execution forces another turn instead of losing a wake. Timer insertion holds
the queue lock while transitioning the job state, and expiry only enqueues a job if it still owns
the timed state. There is at most one strong timer reference per admitted job. Paused jobs stop
registering timers. Stop joins machine reclamation and tests check timer storage returns to zero.

The first 144 paced runs (`paced-opus-matrix.jsonl`, `paced-pcm-matrix.jsonl`) compare original,
refactored and scheduled playback at budgets one/eight and workers one/four. All audio matched,
all clocks took at least the audio duration (within a 50 ms edge tolerance), every scheduled session
was admitted, and no fallback qualified as performance evidence. They exposed repeated early
wakes: consumer drains woke producers waiting only for a future clock deadline. Some cases
registered nearly two deadlines per actual expiration.

`2be796c` separates capacity and pacing waker registrations without changing queue sizes or audio.
Data consumption wakes a capacity wait; closure, failure, flush, cancellation and explicit controls
wake pacing waits. A deterministic test uses a synthetic future clock and verifies that draining
does not wake a timed producer while consumer closure does. The final 96-run sweep selects budget
eight, workers one/four, 1/128/512 Opus and MP3-to-Opus sessions and 128/512 MP3-to-PCM sessions,
with three repetitions and full original/refactored comparisons. It also records per-session
worst inter-chunk gap excess, maximum lateness, final-chunk time and post-EOF cleanup time.
These extra measurements add common harness work, so do not attribute cross-phase CPU differences
solely to the waker change. Timer registration counts directly verify its intended effect.

Relative delivery jitter uses each session's first observed chunk as its clock origin. It omits
that initial offset; first-chunk latency from gate release is reported separately. It is a host
consumer measurement, not native RTP lateness or audible underruns. The worst gap subtracts the
preceding chunk's media duration from the observed gap. P99 is across delivered chunks within a
process; a three-run median follows. Maximum and per-session worst values are retained because
p99 alone hid an end-of-stream tail affecting fewer than one percent of packets.

### Final paced results at 512 live sessions

All final scheduled rows have 512 admitted/live scheduled sessions at gate release, zero fallback
counts, zero retained timers after stop and a timer peak no larger than the admitted count.

| Workload         | Backend/workers | CPU ms/audio s | First chunk p99 ms | Relative jitter p99 ms | Worst excess gap ms | Last chunk s |
| ---------------- | --------------- | -------------: | -----------------: | ---------------------: | ------------------: | -----------: |
| music3.mp3→f32le | dedicated/0     |          2.526 |                5.1 |                   10.7 |                13.1 |        2.988 |
| music3.mp3→f32le | original/0      |          2.515 |                5.2 |                    8.5 |                13.1 |        2.989 |
| music3.mp3→f32le | scheduled/1     |          1.448 |                8.4 |                    5.7 |                 7.5 |        2.991 |
| music3.mp3→f32le | scheduled/4     |          1.692 |                2.9 |                   13.8 |                23.4 |        2.983 |
| music3.mp3→opus  | dedicated/0     |          7.546 |               11.6 |                    9.8 |                35.2 |        2.978 |
| music3.mp3→opus  | original/0      |          7.579 |               10.5 |                   13.7 |                29.5 |        2.978 |
| music3.mp3→opus  | scheduled/1     |          4.926 |               58.1 |                 3729.8 |               232.6 |        6.776 |
| music3.mp3→opus  | scheduled/4     |          5.064 |               15.7 |                    8.4 |                24.2 |        2.988 |
| music3.opus→opus | dedicated/0     |          2.836 |                3.8 |                    6.2 |                94.5 |        3.091 |
| music3.opus→opus | original/0      |          2.685 |                3.7 |                    6.5 |                90.5 |        3.088 |
| music3.opus→opus | scheduled/1     |          0.917 |               13.3 |                    1.6 |               428.0 |        3.435 |
| music3.opus→opus | scheduled/4     |          1.248 |                3.2 |                    4.7 |               146.5 |        3.144 |

The final deadline queue rearmed exactly once per expiration in these steady runs: for example,
512 three-second Opus streams had 76,800 registrations and 76,800 expirations. This replaces the
repeated drain-triggered rearming seen in the initial prototype. The raw records also retain
queue contention, OS/startup thread peaks, context switches, RSS and physical footprint for each
case; the complete resource medians are in `paced-final-summary.json`.

The tail metrics fail the continuity target in some configurations despite matching samples and
payloads. At 512 passthrough sessions, one worker has a median worst excess gap of about 428 ms,
versus 94.5 ms for refactored dedicated. Four workers reduce that to about 146.5 ms, still worse.
For 512 MP3-to-Opus sessions one worker is overloaded: relative jitter reaches seconds and the
last chunk arrives around 6.8 seconds for three seconds of audio. Those runs demonstrate overload,
not sustainable concurrency. Four workers improve that workload's CPU while delivering its final
chunk around the correct media time, but other cases still regress in initial or tail latency.
MP3-to-PCM with four workers has a worse maximum excess gap than dedicated despite lower CPU;
with one worker this particular PCM workload fares better. There is no universal winning pool size.

No delayed audio is dropped, no samples are skipped, and no quality or buffering setting was
changed to finish earlier. Lower CPU and fewer threads do not establish acceptable continuity.
Pacing is retained only behind the existing experimental feature and explicit Rust selection.
The production backend remains dedicated.

## Diagnostic profile and burst headroom

A separate instrumented archive built by `hybrid-profile-build.sh` times machine actions and
machine destruction. Its CPU/latency rows are marked `profilingOnly` and excluded from acceptance
comparisons. Destruction was initially suspected but the nine `cleanup-profile.jsonl` runs rule
it out as the dominant source: dropping all 512 Opus machines took about 3.1 ms with one worker,
and about 5.3 ms with four. The follow-up archive additionally times `ProcessingTransition::apply`.

`transition-profile.jsonl` records exactly 512 processing transitions for the 512 Opus sources,
with about 341 ms cumulative transition wall time in the one-worker run, versus about 3 ms in
destruction. MP3, already transcoding, has zero such transitions. The Opus edge packet has container
trimming metadata; the common playback implementation must activate/condition PCM processing to
preserve the tail instead of forwarding that packet unchanged. The coincident edge transitions
create a short CPU-work burst despite relatively low average passthrough CPU. The profiling clocks
include descheduling and are not CPU counters, but identify the responsible operation; the data
does not justify introducing a speculative destruction/reaper pool.

A full-host headroom check then compares a fixed ten-worker pool against the same original and
refactored backends, at 128/512 sessions, budgets one/eight, three repetitions: 24 additional matched
runs, with zero fallback. This avoids comparing a four-thread CPU ceiling with dedicated playback
which can use all ten host CPUs during the synchronized burst.

| 512 paced Opus streams          | CPU ms/audio s | Relative jitter p99 ms | Worst excess gap ms | Context switches | Aggregate queue wait ms |
| ------------------------------- | -------------: | ---------------------: | ------------------: | ---------------: | ----------------------: |
| dedicated, workers 0, budget 1  |          2.892 |                    6.6 |                89.5 |           161373 |                     0.0 |
| original, workers 0, budget 1   |          2.757 |                    5.8 |                89.6 |           162736 |                     0.0 |
| scheduled, workers 10, budget 1 |          1.926 |                    4.5 |                92.3 |            52313 |                  2109.8 |
| scheduled, workers 10, budget 8 |          1.495 |                    4.2 |                87.4 |            35735 |                  1090.9 |

Ten workers/budget eight restore the worst gap to approximately the matched dedicated result
while retaining CPU savings. One-action turns spend more CPU and have more queue waiting even
with the extra workers. This supports fixed worker/budget configuration based on both steady
load and codec-transition bursts. It does not prove a ten-worker policy is appropriate on a
2–4-vCPU server or justify automatic resizing. The current convenience constructor still caps
available parallelism at four; its policy was not silently changed based on one host.

## Startup thread accounting and explicit fallback audits

The normal harness opens sessions sequentially before releasing the audio gate. Its low startup
OS-thread peaks do not bound a simultaneous startup burst. A separate Darwin thread-name scanner
uses the installed SDK's `proc_pidinfo` task/thread layouts. `startup-thread-roles.jsonl` observes
CPU roles exactly at the configured 1/4/10 counts and only 1–2 named transient startup workers in
that sequential scenario. Samples can miss shorter threads or catch them before names are assigned.
The scanner has its own syscall overhead and is excluded from CPU/latency acceptance evidence.

The stronger `hybrid_startup_diagnostic` starts concurrent blocking API callers and deliberately
withholds bounded byte-stream input while FFmpeg probes. After every engine startup thread is
observed, stdin releases the fixture and every session must finish with identical positive sample
counts. Caller driver threads are named and counted separately. This unsupported-source stress
case is explicitly **not scheduler performance evidence**. Three repetitions at 128 and 512 sources
show the same counts:

| Sessions | Context              | Engine startup threads | CPU workers | Diagnostic caller threads | Main | Total OS threads at barrier |
| -------: | -------------------- | ---------------------: | ----------: | ------------------------: | ---: | --------------------------: |
|      128 | original dedicated   |                    128 |           0 |                       128 |    1 |                         257 |
|      128 | experimental context |                    128 |           4 |                       128 |    1 |                         261 |
|      512 | original dedicated   |                    512 |           0 |                       512 |    1 |                        1025 |
|      512 | experimental context |                    512 |           4 |                       512 |    1 |                        1029 |

All experimental-context burst rows have zero scheduled admissions and exactly N input fallbacks
once probing completes. This demonstrates that startup creation is not bounded by CPU-pool
admission. The scheduler reduces eligible steady-state playback threads, not this startup burst;
caller threads are benchmark scaffolding, not hidden engine savings or engine-only memory costs.
Future source work needs bounded startup admission as well as readiness-driven demuxing.

`fallback-audit.jsonl` separately checks 16 local-file sessions and 16 unsupported MP4/M4A owned
inputs. Both have zero scheduled admissions. Their reasons are respectively input and demuxer,
16 each. Their successful playback validates fallback/counting only and is excluded from every
scheduler comparison table. Capacity and control fallbacks remain covered by deterministic tests.

## Decision and remaining work

The shared scheduler is worth further experiments for qualified workloads; it is not justified
as the production default. The experiment now has real CPU savings, explicit budgets/counters,
bounded readiness-driven realtime deadlines, a measured wake-up optimization and reproducible
continuity regressions/overload cases. It also shows why a smaller pool and lower average CPU can
still fail a burst deadline. Disabling encoder conditioning, discarding trimmed tails, lowering
complexity or shrinking the delivered sample count was not used to make results pass.

Local files, HTTP/plugin sources, Discord transport, crossfade and permanent control migration
remain unchanged and explicitly dedicated. They were investigated above but not enabled: the
current blocking input/control contracts cannot safely run on shared CPU workers, startup remains
unbounded, and pacing/resource qualification is incomplete. Merely flipping eligibility would
create misleading scheduler counts and could stall unrelated sessions. Reversible control
handoff, bounded startup/I/O execution, packet-generation ownership and nonblocking transport
acknowledgements require concrete follow-up implementations with their own comparisons.

No Linux CPU-constrained host, staggered long-duration source mix, live Discord/DAVE traffic,
subjective listening evaluation, native heap inventory or long-duration retention qualification
was available in this run. Synchronized starts and equal-duration fixtures intentionally expose
burst behavior; they are not a representative distribution of track end times. Payload/sample
identity is necessary but does not alone establish audible continuity. No production default,
quality setting, queue limit or cancellation contract was weakened. No push or merge was performed by this agent.

## Verification

Final workspace tests passed with feature enabled (524 tests) and disabled (516 tests), preserving
seven existing ignored manual audits. Workspace/all-target Clippy with `-D warnings` passed both
ways. The addon was regenerated in each configuration and all 108 Node binding tests passed in
each. Docs production build, 60,988 local link checks and API-export validation passed. As before,
local Homebrew linking emits the existing missing-arm64 QTKit warning and is not a static
shipping-artifact audit. New diagnostic examples are compiled in all-target checks and were run
manually; normal tests do not depend on stdin, network services or these resource stresses.

### Supplemental reproduction commands

Use the matching source milestone for historical phases; the current builder runs current code.
The original archive only receives the common benchmark examples and unused cfg declaration.
Binary and fixture identities for every matched sweep are in adjacent `.manifest.json` files.

```sh
python3 benchmarks/hybrid_ab.py \
  --original /tmp/remedia-hybrid-tail/original-target/release/examples/hybrid_scheduler_benchmark \
  --dedicated /tmp/remedia-hybrid-tail/dedicated-target/release/examples/hybrid_scheduler_benchmark \
  --scheduled /tmp/remedia-hybrid-tail/scheduled-target/release/examples/hybrid_scheduler_benchmark \
  --fixtures /tmp/remedia-hybrid-tail/fixtures/music3.opus,/tmp/remedia-hybrid-tail/fixtures/music3.mp3 \
  --pacing realtime --workers 1,4 --budgets 8 --counts 1,128,512 \
  --output /tmp/paced-comparison.jsonl
python3 benchmarks/hybrid-ab-summary.py /tmp/paced-comparison.jsonl --output /tmp/paced-summary.json
benchmarks/hybrid-profile-build.sh /tmp/remedia-hybrid-profile 2be796c
/tmp/remedia-hybrid-profile/target/release/examples/hybrid_scheduler_benchmark \
  /tmp/remedia-hybrid-tail/fixtures/music3.opus 512 1 opus 8 bytes realtime
python3 benchmarks/hybrid-thread-roles.py \
  --original /tmp/remedia-hybrid-tail/original-target/release/examples/hybrid_scheduler_benchmark \
  --scheduled /tmp/remedia-hybrid-tail/scheduled-target/release/examples/hybrid_scheduler_benchmark \
  --fixture /tmp/remedia-hybrid-tail/fixtures/music3.opus --output /tmp/thread-roles.jsonl
python3 benchmarks/hybrid-startup-burst.py \
  --original /tmp/remedia-hybrid-tail/original-target/release/examples/hybrid_startup_diagnostic \
  --scheduled /tmp/remedia-hybrid-tail/scheduled-target/release/examples/hybrid_startup_diagnostic \
  --fixture crates/remedia-core/tests/fixtures/generated-tone.mp3 --output /tmp/startup-burst.jsonl
```

The role scanners use Darwin SDK layouts and are diagnostic-only; the main matched runner also
supports Linux process-status sampling. Linux behavior itself remains untested here.

## Branch history note

During the run, an external `git pull origin main` added merge `d857e94` with the empty release
commit `8ea1bb2`. The merge tree is exactly identical to its first parent `f60e405`
(`2c41c05942e70384e40f7f0a667b28b72c72e86c`), so it changed no tested code. It was preserved rather
than rewriting concurrent work. Local `main` remains `55b71e55b38722401dbd6d0011ccf31b0337000c`.
The agent did not run that pull/merge and did not push any commits.
