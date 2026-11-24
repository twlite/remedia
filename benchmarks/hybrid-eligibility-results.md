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

## Linux environment

Docker Desktop is available on this macOS arm64 host. Pulling
`rust:1.99.1-slim-trixie` failed at Docker Hub DNS resolution (`lookup registry-1.docker.io:
no such host`). The cached published Remedia runtime image contains neither a Rust compiler nor
native development headers. No Linux benchmark results are claimed from that image.

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
CPU-only volume, tempo, biquad, pan and resampling/timeline filters listed in the Fumadocs guide.
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
