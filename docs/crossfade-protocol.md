# Crossfade protocol and implementation plan

Crossfade is **not yet available for playback**. The server's `audio.crossfade.enabled`
setting is a permission flag, not proof that the native dual-source engine is ready.
`node.info.capabilities.crossfade.available` must remain `false` until the full
native implementation and both transport adapters have passed integration tests.
Clients must check `available === true` before requesting crossfade.

## Configuration and negotiation

Server configuration:

```json
{
  "audio": {
    "crossfade": {
      "enabled": true,
      "durationMs": 5000,
      "prepareBeforeMs": 30000,
      "curve": "equalPower"
    }
  }
}
```

The feature defaults to disabled. Valid duration: 1-30 seconds. Preparation
lead: 5-120 seconds *before the transition*, not before the current song ends.

On remote nodes, `node.info` includes the configured and available flags
and validated defaults. Both server permission and session opt-in must be true
before enabling any decoder staging. Embedded nodes must use the owning
client's session configuration as the policy source, without remote server
configuration. The `@remedia/client` node snapshot exposes capability data;
clients should treat absent or malformed data as unsupported.

## Intended protocol (not implemented yet)

1. `guild.create` takes optional `crossfade` session options. Reject
   explicitly requested crossfade when the server capability is unavailable.
2. The native playback engine emits a generation-bound `prepare_requested`
   event before the transition. Ignore missing acknowledgments.
3. The client resolves the queue entry and invokes `stageNext` with
   `expectedGeneration`, unique `entryId`, `queueRevision`, and
   `sourceRevision`. Same identity/revision is idempotent. A newer revision
   invalidates an older stage before opening its replacement.
4. The engine opens a second decoder and prebuffers a bounded amount of PCM.
   Audio is mixed at 48 kHz interleaved stereo using an equal-power envelope.
   The two decoders share one output and one Discord voice transport.
5. When entering a mixed region, switch Opus passthrough into PCM mode.
   When the transition ends, promote the second decoder without closing
   the underlying output, then emit generation-aware lifecycle events.
6. `playbackCompleted` for the outgoing generation carries optional
   `continuation: { type: 'promoted', generation, entryId, transitionId }`.
   The incoming generation also emits `playbackStarted`, so clients do not
   call `play()` again.
7. Stop/seek invalidates the prepared source; pause preserves both decoder
   positions; resume rechecks source expiry. Live/unknown-duration input
   cannot schedule an automatic end-based crossfade.
8. Reconnect snapshots must include both active and staged identities, with
   generation correlation and deduplication of late events.

## Required tests before availability can become true

- Disabled sessions behave identically to the current single-source engine.
- Mixed frames have correct sample continuity, envelope shape, and bounded
  headroom for unrelated and correlated inputs.
- No duplicate output, source resolution, start/completion events, or queue
  promotion during overlapping generations.
- Opus passthrough transitions and filtergraph, volume, pause/resume, seeks,
  stops, and source failures preserve playback correctness.
- Concurrent stage replacement, cancellation, and slow source opening cannot
  promote a stale entry.
- Two bots in one guild maintain independent staging.
- Embedded and remote adapters observe the same lifecycle behavior.
- Missed WebSocket events, reconnect/resume, and server restart reconcile
  without replaying an already-promoted source.
- Expiring URLs and long SABR preparation fall back without stalling output.
