# Resource audit: independent integration verification

## Baseline and scope

The audit started from local and remote `main` at `1980846a515cd650958cef3b85c8864d93d7960f`.
Six GPT-6 Luna agents at maximum reasoning effort worked in isolated checkouts.
The integration branch is `perf/resource-efficiency-audit`. Commits have no coauthor trailers.

Existing Phase 2/3 work already limits native filtergraph threads, reuses resampler storage,
shrinks encoded packet allocations, and preserves eligible Opus passthrough. Those changes,
Opus complexity 9, the existing playback worker model, and release compiler settings are retained.

## Completion patch review and independent checks

The orchestrator reviewed completion state publishers, registration, cancellation, replacement,
and lock ordering. Registration holds playback state before the waker registry; notifications
release both locks before calling executor wakers. Pending futures unregister on drop.
The five async completion lifecycle tests passed independently, including a registration/stop race.
Crossfade natural completion requires the corresponding notification hook during integration.

The original benchmark emitted two constant registry-count fields. They were removed from the
harness, runner, and raw data: actual task-waker reference counts and unit-test registry checks
provide the retention evidence. Blocking timing includes thread joins, whereas async timing ends
when every future is ready. Neither timing is an isolated measurement of Tokio notification latency.

The independent release run used three timing repetitions and three separate Rust-allocation
repetitions for each strategy at 1, 8, 32, 64, 128, 256, and 512 waiters: 84 fresh processes.
All submitted exactly 960 sample frames per channel, finished output, completed every waiter,
and retained zero task wakers after completion. At 512 waiters the OS thread counts were
514 for blocking and 2 for async in every repetition; median peak RSS was 22.12 MiB and
13.56 MiB respectively. Timing varied substantially while other audit work was active, so
this independent run confirms thread occupancy and cleanup rather than a precise latency gain.
These are many waiters on **one** playback session, not 512 concurrent playback sessions.
Native heap allocations were not measured, and process RSS is not a leak detector.

Raw independent results: [resource-audit-independent-waiters.jsonl](resource-audit-independent-waiters.jsonl).
The original strategy comparison and reproduction command are in [resource-audit-1.md](resource-audit-1.md).

The standalone playback baseline executable was built at integration revision `4d17c01` after
the additive completion change, before integrating audio-path changes. This harness uses the
synchronous completion API; it therefore does not measure the async adapter improvement.
The preserved binary SHA-256 is `ee06793c9c2aeb1799db0704a505952a78fbc18e5f71d38dda11b718b47412f3`.
The command-local build settings were:

```sh
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig \
RUSTFLAGS='-L native=/opt/homebrew/opt/lame/lib -L native=/opt/homebrew/opt/svt-av1/lib -L native=/opt/homebrew/opt/openssl@4/lib -l c++' \
cargo build --release --locked -p remedia-core \
  --example resource_audit_1 --example concurrent_playback_benchmark
```

This host emitted a warning about the obsolete QTKit framework lacking an arm64 slice;
the build and the independent waiter tests nevertheless completed successfully. Hosted CI's
pinned native libraries and other operating systems remain separate verification requirements.

## Server lifecycle patch review and independent checks

The orchestrator reviewed session-map locking against resume/expiry races, cancellation token
ownership, per-connection request/task retirement, shutdown ordering, and cleanup after response
receivers disappear. A stale generation previously prevented expiry from closing a resumed session
but did not release the expiry future's strong references. Resume and close now cancel that future.
Initiated destructive cleanup is owned by the server before an RPC handler awaits it, and shutdown
joins those tasks before closing plugin hosts. This fixes excess retention within the configured
resume timeout; it is not evidence of a native allocator leak.

At integrated revision `5a3aa12`, independent `cargo test -p remedia-server --locked` passed
107 tests, with the one audit workload ignored by default. This includes local WebSocket/RPC,
resume/expiry, probe/plugin cancellation, occupied-port startup failure, and shutdown tests.
The separately run ignored workload passed three repetitions of 1,000 client-and-guild lifecycle
cycles: closed-session liveness, pending expiry tasks, session-map size, and active players all
returned to zero. Raw results are in
[resource-audit-independent-server.jsonl](data/resource-audit-independent-server.jsonl).

Independent debug timings were 44.487, 42.586, and 42.564 ms for 1,000 cycles. They do not reproduce
the agent's higher final median consistently; neither invocation isolates production latency.
No speedup or stable slowdown is claimed. The workload opens no media or Discord connection,
and does not measure native heap, RSS, file descriptors, or simultaneous WebSocket capacity.
The same command-local native build flags above were used for the independent server checks.

## Discord packet path: independent review and checks

The caller-owned DAVE buffer API was checked against the installed `davey 0.2.0` source: it
uses the same codec, encryption, and Opus silence handling as the allocating API. The orchestrator
also added cleanup for output storage grown before an encryption error. Neither a rejected
packet nor failed encryption advances the RTP sequence, timestamp, or transport nonce.

After that change, the independent release suite passed 48 tests (two benchmark tests ignored).
The separately run packetization workload processed 50,000 packets for each transport cipher
and reported zero Rust allocator calls per packet for both AES-GCM and XChaCha20-Poly1305.
Observed times were 1750 and 2994 ns/packet during other audit activity. No CPU speedup is claimed.
The suite covers DAVE/transport decryption round-trip and oversize rejection; live Discord and
native heap use remain unverified. Opus complexity 9 remains unchanged.

## Exact original-main build

A separate source archive of `1980846a515cd650958cef3b85c8864d93d7960f` was built with the same
release profile, lockfile, and command-local native flags. The standalone playback executable
SHA-256 is `8b951d2eee8317ba4c082cf4709611cb3ba99393447a75d340926289bc539d20`. This exact-main
executable supersedes the early completion-only build as the full-engine baseline.

## Native compressed fixtures

`resource_audit_fixtures` generates deterministic stereo material directly with native FFmpeg
encoders: 60-second Opus, MP3, AAC and a 15-second Opus transition source. It invokes no media
subprocess. Encoder and muxer failures are propagated; output timestamps use the muxer's final
stream time base. All baseline decodes submitted exactly 60 or 15 seconds of media, including
codec delay/trimming. The checked-in fixture manifest records SHA-256, sizes, and those original-main
decode results. Fixture generation reported linked FFmpeg 9.0.2. These synthetic files are
controlled workload inputs, not a broad subjective audio-quality corpus.

Generate them with the same native environment described above:

```sh
cargo run --release --locked -p remedia-core --example resource_audit_fixtures -- /tmp/remedia-audit-fixtures
```

The fixture generator passed targeted Clippy with `-D warnings`; all four generated sources were
decoded through the exact original-main benchmark executable.

## Original-main codec concurrency sweep

The baseline completed all 63 fresh-process cases: Opus passthrough, MP3 transcoding, and AAC
transcoding at 1, 8, 32, 64, 128, 256, and 512 sessions, with three repetitions each. Each session
processed the entire deterministic 60-second input at output complexity 9. Every Opus run produced
exactly 3001 packets per session (including the trimmed edge packet); MP3 and AAC produced exactly 3000. Total submitted audio was exactly 60 seconds per session in every case.

Raw rows: [resource-audit-main-codecs.jsonl](data/resource-audit-main-codecs.jsonl). The existing
`benchmarks/playback.py` runner recorded exact child user+system CPU deltas and sampled process
RSS/thread peaks at approximately 20 ms. These sampled peaks can miss short-lived maxima; the
one-second in-process snapshots are even coarser. All figures include startup and teardown, and
other audit builds may have been active, so later small CPU deltas require an interleaved rerun.
These unpaced controlled-sink cases do not measure RTP send deadlines or audible continuity.

```sh
python3 benchmarks/playback.py \
  --binary /tmp/remedia-audit-main-target/release/examples/concurrent_playback_benchmark \
  --output /tmp/remedia-audit-main-codecs.jsonl \
  --counts 1,8,32,64,128,256,512 --repeats 3 \
  --fixtures /tmp/remedia-audit-fixtures/music60.opus,/tmp/remedia-audit-fixtures/music60.mp3,/tmp/remedia-audit-fixtures/music60.m4a \
  --workloads passthrough --pacing unpaced
```

## Original-main controls and filter baseline

The controls workload completed 42 fresh-process cases: volume and tremolo filtering at the seven
concurrency levels, three repetitions each. Every case completed playback and all explicit
pause/seek/resume calls succeeded. Its initial seek deliberately starts at 100 ms, and some samples
can be emitted before the initial pause; submitted audio therefore varies slightly rather than
being exactly 60 seconds. Raw `audio_s` and packet totals are retained, and comparisons must
normalize CPU by the work actually delivered. These rows cannot be compared as an identical-packet
workload without accounting for that difference.

Raw rows: [resource-audit-main-controls.jsonl](data/resource-audit-main-controls.jsonl). The invocation
used the codec command above with `--fixtures /tmp/remedia-audit-fixtures/music60.opus`,
`--workloads volume,filter`, and `--controls`. It uses the same process sampling limitations.

The subsequent paced seek/transition/stream sweep runs sequentially after this workload.
The existing paced harness ignores some failed random-seek/stage attempts; its output alone
cannot prove that every attempted operation succeeded. Deterministic lifecycle and transition
regression tests remain necessary, and the final comparison must account for this measurement limit.

## Plugin media transfer and queue: implementation follow-up

The plugin-media continuation adds an owned `Bytes` queue ingress path and passes reqwest response
body chunks to it without converting each chunk to `Vec`. The checked-in lockfile resolves
reqwest `0.13.5` and Hyper `1.12.0`; the response chunk is the HTTP DATA-frame `Bytes`. HTTP/1
splits/freezes its read buffer, while HTTP/2 yields frame bytes. Because `Bytes` does not expose
backing capacity, the queue's visible-byte cap is not a strict total-retained-memory bound. A
fitting `Vec` chunk also retains excess vector capacity. Updated Fumadocs and Rustdoc explain both
ownership cases and the one-current-response-chunk backpressure boundary.

The 15-repetition, matched release benchmark verified the same 4,194,304 samples per channel,
1,024 decoded frames, and checksum in both copy and `Bytes` modes. Unmetered throughput ranges
overlapped. A separate five-repetition allocator-instrumented build recorded exactly 8 MiB fewer
producer allocation bytes per 8 MiB fixture for the `Bytes` mode, excluding decoder/native
allocations and retained backing capacity. A separate synthetic 72-process thread matrix measured
queue occupancy, RSS and OS threads. The blocked `spawn_blocking` alternative starved its unrelated
blocking sentinel; an idle async task comparison is only a resource lower bound, not a functional
transfer implementation. All measured queue bytes drained and all transfer threads returned to
baseline. RSS remained elevated after queue release in some cases, so RSS is not treated as an
object-retention or native-heap measurement. The matrix covered up to 128 streams, below the
configured maximum.

Focused core tests passed repeated producer/reader cancellation cycles. Local server integration
tests passed repeated stop/disconnect, stalled-response timeout, and truncated-response cleanup.
The parent reports that 63 codec cases against the exact original-main baseline passed with exact
packet and sample totals; broader whole-engine verification remains pending. That baseline is
separate: this byte-ingress benchmark does not emit RTP packets, so its correctness check is
decoded samples, frames, and checksum rather than packet totals.

See [plugin-media-transfer.md](plugin-media-transfer.md) for full tables, limitations, and
reproduction commands. Raw measurements:
[queue time](data/plugin-media-queue-time.jsonl),
[queue allocations](data/plugin-media-queue-allocations.jsonl), and
[thread/resource matrix](data/plugin-media-threads-audit.jsonl).

## Original-main paced baseline

The sequential paced sweep completed all 63 cases (seven concurrency levels, three repetitions,
three workloads): 30-second repeated seeks, 36-second crossfade transitions, and 20-second
bounded producer streams. Every transition case observed exactly two completed transitions per
session, including 1024 at 512 sessions. Every case submitted audio and returned successfully.
The seek and stream cases are wall-time-limited workloads; they do not have identical sample
totals across repetitions, and the seek/stage error-count limitation above still applies.
These are controlled sinks and a plugin-style byte producer, not Discord RTP or HTTP source tests.

Raw data: [resource-audit-main-paced.jsonl](data/resource-audit-main-paced.jsonl). The existing
`resource_matrix.py` runner records per-process kernel CPU/peak RSS, sampled physical footprint,
thread count, startup/command/sink latency percentiles, delivered audio, packets, and transitions.
Its one-second snapshots can miss short-lived thread/footprint peaks; it does not measure native
allocator live bytes.

## Plugin ingress: independent verification

The orchestrator reviewed queue locking, shared allocation lifetime, terminal producer errors,
backpressure, and reader/producer cancellation. A new stress test was corrected to request
cancellation before dropping its reader in cancellation-specific iterations; otherwise the
producer could legitimately observe reader closure first. No production error precedence was changed.

Independent checks passed all ten byte-stream tests (including 2048 blocked-producer and
1024 blocked-reader cycles), 22 loopback plugin-forwarding WebSocket tests, workspace-wide
Clippy with `-D warnings`, Rust formatting, and repository formatting. The Clippy failures in
the newly added quality example were fixed, without suppressing any lint.

The independent 12-case allocation run matched every decoded sample count, frame count, and
checksum. Each paired 8 MiB input removed exactly 8,388,608 producer allocation-request bytes.
Queue occupancy stayed within its visible-byte cap. This confirms reduced queue-ingress allocation,
not a throughput gain or a strict bound on hidden shared backing memory. Raw rows:
[plugin-media-queue-independent.jsonl](data/plugin-media-queue-independent.jsonl).

The existing dedicated transfer thread is retained: the tested blocking-pool candidate could
starve unrelated blocking work, while the async-idle comparison did not transfer or decode media.
No default media queue size was changed without an equivalent stalled-source audio benchmark.

The new `write_bytes` Rustdoc example also passed independently. On this host rustdoc does not
accept rustc's `-l c++` option directly: `RUSTDOCFLAGS` used the same native search paths with
`-C link-arg=-lc++`, alongside the existing `RUSTFLAGS` native settings. This was a command-local
linking correction, without changes to installed dependencies or the distributed build.

## Crossfade integration review and native lifecycle stress

The orchestrator reviewed event publication sites for state/stage lock release before callbacks,
overflow reconciliation, and generation retirement after seeks, promotions and cancellations.
The monitor now captures its initial source mapping before scheduling its task, so queued
transitions do not replace that initial identity before its first poll. Empty revision-waiter
registries release peak storage, and notification takes their existing map instead of allocating
a second temporary vector. A 512-waiter cancellation/notification test checks registry release.
Completion-waker replacement and removal now drop old wakers outside state/registry locks; a
reentrant-drop regression checks that playback state is unlocked when the waker is destroyed.

Independent core/server checks passed: 251 core unit tests, 35 decoder integration tests, one
diagnostics test, the public byte-stream doctest, and 112 server tests. Manual audit workloads
were ignored by the ordinary test run and invoked separately. The Node crate still requires its
owned adapter update for the new `EventsDropped` variant; a complete workspace check must wait
for that integration. Rust exhaustive event matches require a new arm, as documented.

The release native lifecycle workload ran 3000 create/play/stop/drop cycles in one process: 1000
natural completions, 1000 cancellations with a pending async completion waiter, and 1000 failed
startups. Each natural completion submitted all 960 expected sample frames per channel; every
cycle released its worker and its session owner's weak reference became unupgradeable. After
warmup and after all 3000 cycles, the process had two threads and three open handles. Intermediate
100-cycle snapshots stayed within the test's handle/thread bounds. RSS rose from 15,450,112 to
15,843,328 bytes and footprint from 4,375,080 to 4,669,992 bytes; these process measurements do
not inventory native allocations and do not prove or disprove allocator caching. Native heap
bytes were unmeasured. Raw snapshots:
[resource-audit-native-lifecycle.jsonl](data/resource-audit-native-lifecycle.jsonl).

```sh
cargo test --release -p remedia-core --lib --locked \
  resource_audit_native_lifecycle_releases_threads_handles_and_session_owners \
  -- --ignored --nocapture --test-threads=1
```
