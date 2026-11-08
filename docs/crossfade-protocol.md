# Crossfade protocol

Crossfade is not yet available for playback. The server's `audio.crossfade.enabled` option is a permission flag, not proof that the dual-source mixer is operational. Clients must check `node.info.capabilities.crossfade.available` before enabling this feature.

## Configuration

Crossfading is disabled by default. A configured server may advertise its preferred duration and preparation lead, but it must report `available: false` until the native mixer, source promotion, and embedded/remote protocol are implemented and tested.

## Staging and promotion

The intended staging identity contains the current playback generation, a unique queue-entry ID, a queue revision, and a source revision. Repeated requests for the same identity must not open the source again. A newer request invalidates the previous stage. The PCM buffer is bounded to two seconds and preparation is cancelled on seek or stop.

Remedia will send a preparation request before the transition. The client may ignore it, or resolve the next source through its plugin system and submit a stage. Remedia must never choose the next queue entry itself. During crossfade, two decoders feed a single normalized 48 kHz stereo mixer and a single Opus encoder. The incoming source starts its new generation before the outgoing source finishes.

Once promotion is supported, the outgoing completion event will include a continuation reference to the promoted generation and queue-entry ID. Clients must reconcile that event without sending a duplicate play command. A missing or expired stage falls back to existing single-source playback.

## Release gate

Crossfade availability must remain false until the real-time mixer preserves sample continuity, source identity, and event ordering across remote and embedded sessions. Seek, pause, stop, source replacement, Opus passthrough, filters, reconnect, and expiring sources require integration tests.
