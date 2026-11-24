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
already accepted PCM. Native filters may now start on scheduled owned-memory sessions.

Diagnostics report control workers, queued/current handoffs and cumulative handoffs separately
from permanent fallback counters. A temporary handoff is not reported as dedicated playback.
The production dedicated path remains unchanged. Crossfade continues on dedicated execution.
Deterministic regression coverage performs eight seeks and eight graph replacements while
paused, returns to scheduled playback, completes, and compares every Opus payload and sample
position to the dedicated backend. Existing stalled-consumer cancellation and lifecycle stress
also run through the bounded control executor.
