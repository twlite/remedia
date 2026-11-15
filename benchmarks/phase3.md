# Phase 3: Opus pre-skip, seamless packet recovery, packet path, and LTO

Measured on Apple M5 (10 logical CPUs, 24 GiB), macOS Darwin 27 arm64, Rust 1.99.1, FFmpeg 8
libraries from Homebrew, libopus bundled and statically linked (NEON intrinsics compiled in),
Opus 128000 bits/s, complexity 9, VBR, unchanged in every build. The real-file baseline, the after-matrix, the LTO comparison and the unpaced A/B ran with the host
otherwise idle. The original pre-skip-0 baseline rows (`phase3-before-synthetic.jsonl`) overlapped
with low-priority builds and tests, which adds spread to those cells but not a bias that favors the
after build. Commands to reproduce are in [README.md](README.md#phase-3-resource-pass-pre-skip-seek-and-promotion-recovery-packet-path-lto).
Raw per-run rows (without the per-second samples) are in [data/](data/); every cell below is a
median of three runs unless stated.

_Before_ is commit `0491ffd` (the benchmark harness on top of the unchanged engine), no LTO.
_After_ is this branch with `lto = "fat"`. CPU is the process's exact `wait4` user+system time;
_steady_ CPU excludes the first three seconds (session start-up). Peak RSS is the kernel's
`ru_maxrss` for that process. Threads is the largest one-second sample.

## What changed in the numbers

Real FFmpeg/libopus Opus files (pre-skip 312), 20-36 s paced runs. Before this branch these were
never forwarded as packets (passthrough share 0.00).

| Workload                       | Sessions | Steady CPU s          | Peak RSS MiB         | Threads            | Passthrough share |
| ------------------------------ | -------- | --------------------- | -------------------- | ------------------ | ----------------- |
| Opus playback                  | 32       | 7.39 → 1.21 (-84%)    | 36.6 → 25.0 (-32%)   | 34 → 34            | 0.00 → 1.00       |
|                                | 128      | 11.02 → 3.60 (-67%)   | 106.6 → 60.3 (-43%)  | 130 → 130          | 0.00 → 1.00       |
|                                | 512      | 49.37 → 6.69 (-86%)   | 386.5 → 200.5 (-48%) | 514 → 514          | 0.00 → 1.00       |
| Opus + random seek every 3 s   | 32       | 14.01 → 7.89 (-44%)   | 40.2 → 35.2 (-12%)   | 35 → 35            | 0.00 → 0.77       |
|                                | 128      | 23.07 → 16.56 (-28%)  | 111.2 → 100.3 (-10%) | 133 → 134          | 0.00 → 0.76       |
|                                | 512      | 94.04 → 33.99 (-64%)  | 403.1 → 320.8 (-20%) | 522 → 522          | 0.00 → 0.76       |
| Opus, two crossfade promotions | 32       | 17.45 → 7.05 (-60%)   | 125.1 → 86.6 (-31%)  | 123 → 85 (-31%)    | 0.00 → 0.83       |
|                                | 128      | 32.62 → 19.04 (-42%)  | 450.1 → 335.2 (-26%) | 476 → 258 (-46%)   | 0.00 → 0.83       |
|                                | 512      | 138.97 → 63.18 (-55%) | 1729 → 1256 (-27%)   | 2008 → 1506 (-25%) | 0.00 → 0.82       |

The seek and promotion rows remain partly in PCM by design: six frames per seek, the overlap, and
the last second of a track (see "Remaining"). Sink delay p99 stayed at 255-290 µs in every cell.

Workloads that do not use Opus passthrough are the control. They are unchanged within the run-to-run
spread of the paced harness (see [phase3-results.md](phase3-results.md) for every cell): MP3
transcode, AAC transcode, MP3 with a tremolo filter, MP3 from a byte stream, volume-processed Opus,
and the earlier pre-skip-0 Opus files.

The paced runs are noisy at moderate load (identical code moved by ±20-40% between runs), so
build-versus-build claims that could be a few percent use the **unpaced** A/B below.

### Heap counters (Rust allocator only; FFmpeg and libopus allocate natively and are not counted)

32 sessions, one run each, allocation counting enabled (so CPU is not compared in these rows).

| Workload                       | Allocations / audio s | Bytes allocated / audio s  | Peak live heap KiB     |
| ------------------------------ | --------------------- | -------------------------- | ---------------------- |
| real Opus                      | 153 → 103 (-33%)      | 403 401 → 22 071 (-95%)    | 1209 → 965 (-20%)      |
| real Opus + seeks              | 158 → 120 (-24%)      | 405 151 → 117 841 (-71%)   | 1731 → 1610 (-7%)      |
| real Opus + promotions         | 307 → 216 (-30%)      | 1 266 031 → 544 574 (-57%) | 55 320 → 27 713 (-50%) |
| MP3 transcode (unchanged path) | 137 → 137             | 402 860 → 402 887          | 1223 → 1223            |

### Packet path (`cargo test --release -p remedia-discord ... --ignored`)

|                                                         | Before       | After                                              |
| ------------------------------------------------------- | ------------ | -------------------------------------------------- |
| RTP datagram, heap allocations per packet               | 5.00         | 1.00 (DAVE's own output in `davey`)                |
| RTP datagram, AES-256-GCM                               | 1.05-1.14 µs | 0.95-1.03 µs                                       |
| RTP datagram, XChaCha20-Poly1305                        | 1.40-1.41 µs | 1.30 µs                                            |
| `RoutedOpusSender::send_packet`, allocations per packet | 1.39         | 0.37-0.39 (the channel's own)                      |
| `RoutedOpusSender::send_packet`, time                   | 1.07-1.14 µs | 1.10-1.16 µs (unchanged: dominated by the channel) |

## Opus pre-skip

_Why it mattered._ Every file written by libopus or FFmpeg has a 312-sample pre-skip, so none could
use passthrough: the rows above with `0.00` before. Findings on real files:

- Decoding a file with its pre-skip header zeroed gives output **bit-identical** to the normal
  decode, shifted by exactly 312 samples (maximum difference 0.0 on four sources). Forwarding
  the packets unchanged therefore reproduces the reference audio exactly, 6.5 ms later.
- The 312 pre-roll samples are not always silent: an abrupt start has pre-echo of about -30 dB
  relative to the onset (-32 dBFS for full-scale noise and music, -63 dBFS for a sine). Remedia's
  own encoder produces the same pre-echo for any PCM stream, so forwarding the first packet is no
  worse than what the PCM path emits. A gate (pre-roll at most -60 dBFS, or at most 10% of the audio that
  follows) keeps mislabeled streams on the decoder.
- Ogg carries the pre-skip as `skip_samples` on the first packet (pts -312; later packets at
  648 + 960k). Matroska/WebM carries it only in the codec parameters, with millisecond timestamps.
  Both are handled and tested with real FFmpeg-written files.

### Why the first implementation of seek/promotion recovery was wrong, and the fix

A fresh libopus encoder delays its output by its 312-sample lookahead. Re-encoded PCM therefore
lags the packet timeline by 312 samples. The recovery that shipped in the first commit switched
to packets right after the first re-encoded frame. Measured on real files with a fresh libopus
decoder as the client, against FFmpeg's decode (residual energy relative to the signal, per 20 ms
block after the switch):

| Switch to packets                                                                              | Block 1                          | 2   | 3   | 4   | 5   | 6   | 7   | 8   |
| ---------------------------------------------------------------------------------------------- | -------------------------------- | --- | --- | --- | --- | --- | --- | --- |
| Immediately after the first frame (first commit)                                               | -0.9 dB (audio 303 samples late) | -5  | -12 | -18 | -25 | -33 | -39 | -45 |
| A client decoder cold-started at an original packet (inherent)                                 | -2                               | -6  | -10 | -14 | -22 | -28 | -34 | -41 |
| Encoder warm ≥5 frames, lookahead drained with the next packet's first 312 samples (simulated) | -23                              | -30 | -37 | -43 | -49 | -58 | -65 | -70 |
| **Implemented**, seek to a packet boundary                                                     | -33                              | -42 | -48 | -53 | -58 | -64 | -71 | -77 |
| **Implemented**, seek inside a packet                                                          | -18                              | -23 | -29 | -35 | -41 | -45 | -52 | -60 |
| **Implemented**, crossfade promotion                                                           | -25                              | -29 | -34 | -40 | -45 | -53 | -58 | -64 |

The encoder's own re-encoding noise for this synthetic source is -17 to -24 dB, so the seam adds
no artifact beyond it. A warm-up shorter than five frames is clearly worse (one frame: -7 dB), which is why
recovery waits six frames. The implemented junction only runs for streams whose pre-skip equals
the encoder lookahead; every other stream keeps its previous behavior after a seek or promotion.

## LTO

Runtime difference on the unchanged code and on this branch, using **unpaced** alternating A/B (12
repetitions, exact `wait4` CPU, ± is half the min-max spread). Paced runs could not resolve it.

| Workload                                          | base    | base + fat   | after, no LTO | after + thin  | after + fat   |
| ------------------------------------------------- | ------- | ------------ | ------------- | ------------- | ------------- |
| MP3 transcode, 4 sessions                         | 1.062 s | -2.0% (±7%)  | -2.3% (±5%)   | -1.6% (±1%)   | -1.9% (±2%)   |
| AAC transcode, 4 sessions                         | 0.993 s | -2.3% (±5%)  | -2.7% (±1%)   | -2.4% (±4%)   | -2.5% (±6%)   |
| MP3 + tremolo, 4 sessions                         | 1.117 s | -1.1% (±2%)  | +3.2% (±2%)   | -1.2% (±2%)   | -1.1% (±5%)   |
| Real Opus, 4 sessions (PCM before, packets after) | 1.250 s | -0.3% (±4%)  | -97.7% (±6%)  | -97.7% (±20%) | -97.8% (±13%) |
| Pre-skip-0 Opus, 128 sessions (packet path)       | 0.390 s | -1.8% (±11%) | +11.8% (±12%) | -0.1% (±15%)  | -0.6% (±11%)  |

LTO is **marginal at run time** (about 1-2%, at or inside the noise), and thin and fat are
indistinguishable. This is expected: libopus (77% of active CPU in a sampled profile) and the
FFmpeg libraries are built outside Rust's LTO scope, and the Rust code is about 9%. The packet-path
row also shows the branch did not slow the unchanged passthrough path.

The cost and benefit are in the build instead: clean `cargo build --release -p remedia-server --bins`
(no other change):

|        | Wall time | `remedia-server` | `remedia-server-runtime` |
| ------ | --------- | ---------------- | ------------------------ |
| No LTO | 33 s      | 6.9 MB           | 21.4 MB                  |
| Thin   | 45 s      | 6.0 MB           | 18.8 MB                  |
| Fat    | 79 s      | 4.5 MB           | 13.4 MB                  |

Fat LTO was requested and is kept: it gives the smallest binaries (-37% against no LTO and -29%
against thin on the runtime) for +34 s over thin. Because the run-time benefit is not measurable,
thin LTO is a reasonable alternative if CI link time matters more than binary size
(`lto = "thin"` in the root `Cargo.toml`). CI jobs that run `cargo test --workspace --release` now link
with fat LTO.

## Memory owners of a staged session

The earlier reported ~2.4 MiB per staged session (1873 MiB at 512 sessions) was attributed with
`vmmap`, `footprint` and `heap` at 64 sessions twelve seconds into the transition workload (all
sessions staged), before any buffer size was changed. Physical footprint was 154 MB, of which
143 MB was `Malloc Small` dirty pages and 8.3 MB stacks (324 stack regions); a passthrough-only run
of the same size was 19.7 MB.

`heap -sortBySize` size classes at 117.6 MB of live malloc nodes:

| Owner                                                                                                                     | Evidence                                                               | Per session                         |
| ------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- | ----------------------------------- |
| Staged PCM buffer (`StagedPcmBuffer`)                                                                                     | 64 nodes of 816 KB                                                     | **816 KiB (about 44% of the heap)** |
| Two FFmpeg inputs and decoders (the outgoing track's PCM decoder and the stage's)                                         | 80 KB ×4, 64 KB ×2, 48 KB ×3, 32 KB ×2, 28 KB ×2, 16 KB ×2 and smaller | about 410 KiB each, 820 KiB         |
| Decoder and resampler for the outgoing track (inferred: the 80 KB and 48 KB node counts halve when the file is forwarded) | present only because the file was decoded                              | removed when it is forwarded        |
| Thread stacks                                                                                                             | 8.3 MB over 64 sessions                                                | about 130 KiB                       |

Changes made after attributing: the buffer's `VecDeque` reserves exactly its bounded capacity
(752 KiB instead of 816 KiB; doubling growth would settle at 1 MiB and the ring touches all of it);
the outgoing real Opus file is forwarded instead of decoded, which drops its decoder and resampler;
and a promoted local Opus file releases the stage's thread and buffer for the rest of the track.
At the same moment of the same workload the footprint went from 154 MB to 125 MB (-19%) and the live
Rust heap from 117.6 MB to 88.2 MB. The buffer _size_ (2 s, with a 1 s readiness threshold) was
not changed: it is the stall cushion for network sources.

## Rejected after measurement

- **PCM allocation tuning (decoder frame reuse, filtergraph frames, packet scratch).** In the
  sampled profile (32 paced MP3 sessions) libopus is 77% of active CPU, Rust code including the
  benchmark sink 9%, and the FFmpeg libraries about 2%. The Rust allocator is called about 137
  times per audio second per session and the MP3 path's counters are unchanged by this branch.
  Even removing every Rust allocation could not move the total by more than a few percent, so none
  was done.
- **A bounded readiness-driven scheduler to replace the thread per player.** A thread costs about
  0.36 MiB all-in per passthrough session (200 MiB / 512) and about 1.5 ms of CPU per second at 128
  sessions in the paced harness, mostly the harness's own timer wake-ups; the engine's per-packet
  cost unpaced is 3 µs. Paced CPU is 4-5× the unpaced cost of the same work (19 ms versus 4.2 ms per audio second for MP3)
  because of low-load clock scaling and timer wake-ups, which a different scheduler on the same
  host cannot remove. No candidate showed a measurable gain, so none was built.
- **Rebuilding libopus.** The bundled static libopus is already as fast as Homebrew's: FFmpeg CLI
  encoding 60 s of this signal takes 0.32 s of CPU, while Remedia's whole MP3 decode, resample and
  Opus encode of the same 60 s takes 0.25 s.
- **Lowering Opus complexity or bitrate.** Not done, as instructed.

## Remaining and worth investigating

- **Network and plugin-stream sources** keep their stage thread and 2 s PCM buffer after promotion
  to preserve the stall cushion. A compressed-packet lookahead (about 32 KB per 2 s of Opus)
  would give the same cushion for 1/24 of the memory and no PCM decode.
- **The last second of a track** whose duration is not on the packet grid is decoded when no next
  track is staged, because a packet that would cross the (never used) overlap boundary disables
  passthrough for the rest of the file. Pre-existing; visible now that real files are eligible
  (the final ~50 packets of the promoted file in the tests).
- **Gain and filter transitions** (`prepare_passthrough` after a volume or filter change) still
  switch back to packets without draining the encoder lookahead, so they keep the short artifact
  the docs already describe. The same junction would fix it for 312-pre-skip streams.
- **Pre-skip other than 312 or at least one packet** (older opusenc used 3840) still decodes.
- Plugin media transfer uses one OS thread per stream; its cost was not measured here.
- A pre-existing timing test (`separate_sessions_progress_independently_under_backpressure`) is
  flaky: with the whole `playback::` suite running in parallel it failed 1 of 210 iterations at the
  base commit and 4 of 260 on this branch (not distinguishable); in isolation it passed 150 of 150
  on both. It waits on a process-wide diagnostics channel and is likely a tracing
  callsite-registration race. It was not changed.
