# Resource audit 6: server lifecycle ownership

## Reproduction

The ignored in-process workload performs 1,000 client-session and guild lifecycles per repeat:
create, detach, resume, and close. It records elapsed time, the number of expiry tasks still alive,
and how many closed `ClientSession` values remain upgradeable through `Weak` references. The
server-session map and active-player count are checked after each repeat.

Run it with the same local native-link environment used for both measurements:

```sh
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig \
LIBRARY_PATH=/opt/homebrew/opt/lame/lib:/opt/homebrew/opt/svt-av1/lib:/opt/homebrew/opt/openssl@4/lib \
RUSTFLAGS='-C link-arg=-lc++' \
  cargo test -p remedia-server --lib \
  resource_audit_6_measures_thousand_session_lifecycle_cycles -- --ignored --nocapture
```

This host used Rust 1.99.1 / Cargo 1.99.1, macOS 27 arm64, FFmpeg 9.0.2 through the explicit
pkg-config directory, and Cargo's unoptimized test profile. `LIBRARY_PATH` and the C++ link flag
select existing local libraries needed by this Homebrew FFmpeg installation; no packages were
installed or changed. The workload does not open media or call FFmpeg, so it has no packet or
sample output to compare.

## Measurements

Each repeat creates a fresh `ServerInner`; all 1,000 cycles also create and close one guild. The
before sample has three repeats. The after sample has nine repeats across three invocations of the
command. The expiry-task counter and weak-reference probes exist only in `cfg(test)` code. These
figures measure Rust object liveness, not Rust heap bytes, native heap bytes, RSS, or allocator
retention.

| State  | Elapsed median (min–max) for 1,000 cycles | Closed sessions still live | Expiry tasks still live | Session map | Active players |
| ------ | ----------------------------------------: | -------------------------: | ----------------------: | ----------: | -------------: |
| Before |                 42.996 ms (41.846–51.137) |                      1,000 |                   1,000 |           0 |              0 |
| After  |               106.638 ms (50.551–304.454) |                          0 |                       0 |           0 |              0 |

Raw runs are in [`data/resource-audit-6.jsonl`](data/resource-audit-6.jsonl). The after median is
higher, but the after samples also vary widely. This suggests a possible debug-profile slowdown;
these synthetic measurements are too noisy to estimate its size or project it to production. The
task and session-retention counts are deterministic.

## Findings and scope

- A disconnect created one sleeping expiry task holding both `ServerInner` and `ClientSession`.
  Resume changed a generation, but the old task still held both values until the original deadline.
  A fast disconnect/resume/close cycle therefore accumulated stale tasks and session objects.
- Resume, detach, and expiry now serialize through the session map. Resume or close cancels the
  stored expiry token; expiry checks and removes the session atomically against a concurrent resume.
  Expiry waiters and initiated resource cleanup are server-owned tasks, completed entries are reaped
  as new tasks arrive, and server shutdown joins the remaining tasks before closing plugin hosts.
- Each WebSocket owns its RPC `JoinSet`, event forwarder, and writer lifecycle. Requests are reaped
  as they finish and aborted/joined when the socket closes. Writer sends and inline responses stop
  waiting when the connection or server shuts down. Probe and plugin cancellation tokens remain
  connection-scoped, and existing ownership checks are unchanged.
- Cleanup started by `client.close` or `guild.destroy` is held in the same server-owned task set.
  Shutdown drains it before closing plugin hosts, including when the RPC receiver has gone.
  Shutdown also completes session and telemetry cleanup if the accept task returns an error.
- The event broadcast channel and console/diagnostic queues remain bounded by their existing
  configuration and capacities. `remedia-observability` needed no code change.
- No timer wheel or polling scheduler was added. A per-session timer remains the simplest expiry
  mechanism; this change addresses stale timers after resume/close, and the test does not establish
  a production need for a different scheduler.

The stress workload is direct state lifecycle churn, not 1,000 simultaneous WebSocket clients, active
Discord voice sessions, or audio playback. The integration suite separately exercises session
resume/expiry, hung plugin request cancellation on socket close, failed startup on an occupied port,
and server shutdown with active sessions.
