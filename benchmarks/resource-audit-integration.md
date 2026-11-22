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
