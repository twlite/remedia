# Fallback seeking

Measurements for `PlaybackSession::configure_fallback_seeking`: what a seek on a source that cannot
seek costs when it is served by a replacement decoder, and whether playback keeps delivering on time
while the replacement is prepared. The mechanism is documented in
[fallback seeking](../apps/docs/content/docs/core/playback.mdx).

## Reproduction

```sh
export FFMPEG_DIR="$(brew --prefix ffmpeg)" PKG_CONFIG_PATH="$FFMPEG_DIR/lib/pkgconfig" DYLD_LIBRARY_PATH="$FFMPEG_DIR/lib"

# Fifteen-minute test tracks. This uses the ffmpeg CLI only to make the input; the engine never does.
ffmpeg -f lavfi -i "anoisesrc=d=900:c=pink:r=48000:a=0.2" -ac 2 -c:a libopus -b:a 128k long.ogg
ffmpeg -f lavfi -i "anoisesrc=d=900:c=pink:r=48000:a=0.2" -ac 2 -c:a libmp3lame -b:a 192k long.mp3

cargo build --release -p remedia-core --example fallback_seek_benchmark
target/release/examples/fallback_seek_benchmark long.ogg --targets-ms 500,5000,60000,300000,780000
target/release/examples/fallback_seek_benchmark long.ogg --targets-ms 500,5000,60000,300000,780000 --rate-mbit 100
```

`crates/remedia-core/examples/fallback_seek_benchmark.rs` serves the track from an in-process HTTP
origin that ignores `Range` (optionally limited to `--rate-mbit`), plays it through a real-time
paced output that keeps 200 ms queued like a voice sender, waits two seconds, then seeks and watches
the following 300 ms. It runs each target three ways: the same origin without the option, the same
origin with it, and a local file, where a seek is a demuxer call. One JSON object is printed per
measurement. `data/fallback-seeking.jsonl` holds the raw output of the runs below.

`latencyMs` is the time `seek()` blocked: for fallback seeking, until the replacement is playing.
`cpuSeconds` is process user+system time over that call. An **underrun** is a frame delivered after
the output's queue had emptied. `worstGapMs` is the longest interval between two frames reaching the
output; a frame is 20 ms of Opus or 24 ms of MP3, so a value near that means delivery was not
interrupted.

## Environment

Apple M5, 10 logical CPUs, macOS Darwin 27 arm64, Rust 1.99.1, release profile, FFmpeg 9.0.2 from
Homebrew. Loopback network, one run per row, host not isolated. Treat the timings as exploratory;
the counts (connections, bytes, underruns) are deterministic. Seeks are forward from about 2.2 s
except 0.5 s, which is a rewind.

## Results

### Without the option, a source that ignores `Range` cannot be sought safely

| Track (unlimited loopback) | Seek                                                                                                                                                                                                                                                  |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Opus, forward 5-780 s      | All five failed with `UnsupportedSeeking` after 4-7 reconnects and **29-36 MB** served for a 9.8 MB file. No frame reached the output in the following 300 ms and the reported position stayed at 2.2 s: the attempt had disturbed the read position. |
| Opus, rewind to 0.5 s      | One run reported success while playback continued at 5.5 s; another landed on 0.5 s. FFmpeg's behavior depends on what it had buffered.                                                                                                               |
| MP3, forward 5-780 s       | Succeeded by reading and discarding the skipped bytes **on the playback worker** (20 MB at 780 s), so a slow origin stalls delivery for as long as the transfer takes.                                                                                |

### With the option

Opus, 128 kbit/s, 15 minutes, `--rate-mbit` as stated:

| Target | Unlimited latency | CPU   | Origin bytes | 100 Mbit latency | CPU   | Origin bytes | Underruns | Worst gap |
| ------ | ----------------- | ----- | ------------ | ---------------- | ----- | ------------ | --------- | --------- |
| 0.5 s  | 30 ms             | 6 ms  | 2.0 MB       | 31 ms            | 5 ms  | 0.8 MB       | 0         | 20.5 ms   |
| 5 s    | 34 ms             | 6 ms  | 0.9 MB       | 29 ms            | 4 ms  | 0.8 MB       | 0         | 21.0 ms   |
| 60 s   | 31 ms             | 9 ms  | 2.4 MB       | 73 ms            | 14 ms | 1.9 MB       | 0         | 20.5 ms   |
| 300 s  | 30 ms             | 11 ms | 4.8 MB       | 290 ms           | 35 ms | 3.9 MB       | 0         | 21.2 ms   |
| 780 s  | 50 ms             | 34 ms | 9.8 MB       | 711 ms           | 86 ms | 9.1 MB       | 0         | 21.2 ms   |

MP3, 192 kbit/s, 15 minutes:

| Target | Unlimited latency | CPU   | Origin bytes | 100 Mbit latency | CPU   | Origin bytes | Underruns | Worst gap |
| ------ | ----------------- | ----- | ------------ | ---------------- | ----- | ------------ | --------- | --------- |
| 0.5 s  | 29 ms             | 3 ms  | 1.8 MB       | 29 ms            | 5 ms  | 0.8 MB       | 0         | 25.1 ms   |
| 5 s    | 34 ms             | 3 ms  | 0.8 MB       | 32 ms            | 6 ms  | 0.8 MB       | 0         | 24.8 ms   |
| 60 s   | 34 ms             | 8 ms  | 2.3 MB       | 127 ms           | 9 ms  | 2.2 MB       | 0         | 24.3 ms   |
| 300 s  | 30 ms             | 17 ms | 9.0 MB       | 605 ms           | 40 ms | 7.9 MB       | 0         | 25.2 ms   |
| 780 s  | 53 ms             | 34 ms | 20.7 MB      | 1517 ms          | 93 ms | 19.4 MB      | 0         | 25.1 ms   |

For comparison a local file seeks in 5-30 ms with 0-23 ms of CPU, and the same MP3 seek in place
on the range-ignoring origin took 5-34 ms but on the playback worker.

### Without the option, after the disabled-mode fix

The Opus rows above were measured before a seek that cannot be done stopped being handed to FFmpeg.
Rerun at 100 Mbit/s (`data/fallback-seeking-opus-recovery.jsonl`, `run: disabled-mode-rerun`), the
same origin without the option now behaves like this:

| Seek (Opus, 15 min) | Result                                                                                              |
| ------------------- | --------------------------------------------------------------------------------------------------- |
| Rewind to 0.5 s     | Refused in 4 ms with `UnsupportedSeeking`; no connection; 15 frames kept flowing in the next 300 ms |
| Forward to 5 s      | Applied in place in 18 ms; no reconnect                                                             |
| Forward to 300 s    | Applied in place in 290 ms with 282 ms of CPU, on the playback worker; no reconnect                 |
| Forward to 780 s    | Applied in place in 680 ms with 677 ms of CPU, on the playback worker; no reconnect                 |

Before, every Opus seek on this origin failed after 4-7 reconnects and ~30 MB of transfer and no
audio followed. The in-place walk decodes every frame it discards, which is why its CPU is about six
times the fallback's (109 ms at 780 s): fallback seeking also skips packets without decoding them.

### Returning to Opus packets after the swap

`--opus` plays the Opus track to an Opus output (a paced packet sink) and measures, around a fallback
seek, how long the stream takes to be back on forwarded packets and what each state costs:

```sh
target/release/examples/fallback_seek_benchmark long.ogg --opus --targets-ms 5000,60000,300000
```

| Target | Swap latency | Back on packets | CPU/s forwarding | CPU/s after recovery | CPU/s transcoding | Underruns |
| ------ | ------------ | --------------- | ---------------- | -------------------- | ----------------- | --------- |
| 5 s    | 27 ms        | 146 ms          | 0.99%            | 1.00%                | 3.88%             | 0         |
| 60 s   | 31 ms        | 150 ms          | 1.00%            | 1.00%                | 3.68%             | 0         |
| 300 s  | 33 ms        | 155 ms          | 0.96%            | 0.97%                | 3.77%             | 0         |

Before recovery was implemented the stream stayed on the PCM path until the next track, which costs the
third column's 3.7-3.9% of a core per playing Opus session, 3.8 times the forwarding cost, for the
rest of a possibly hour-long track.

### What the numbers say

- **Playback is not interrupted.** Every row has zero underruns and a worst inter-frame gap equal to
  the frame duration (to within 1 ms), including the 1.5 s preparation of the throttled MP3.
- **Latency is the origin's transfer time to the target.** At 100 Mbit/s (12.5 MB/s) the Opus case
  moves 9.1 MB in 711 ms and the MP3 case 19.4 MB in 1517 ms: the replacement reads at line rate and
  nothing else adds up. On a fast origin the floor is about 30 ms: a thread, a connection, a
  decoder and the probe.
- **CPU scales with what is decoded, not with the target.** Packets that end more than a second
  before the target are read and dropped without decoding, so a seek to 13 minutes costs 34 ms of CPU
  for both formats and 86-93 ms with the 100 Mbit pacing (more wake-ups). Before packets without a
  timestamp at the very start of a stream were allowed to be dropped, MP3 decoded everything up to
  the target: 448 ms of CPU for the 780 s seek, now 34 ms.
- **Extra cost is one connection and one decoder for the duration of the seek.** The origin serves
  the track from the start once more, up to the target: 20 MB for a 13-minute MP3 seek.
- **The replacement lands on the compensated position.** Each row's `positionAfterMs` is the target
  plus the latency plus the 300 ms observation window, for example 300573 ms for a 300 s target that
  took 290 ms to prepare.

## Not measured

- A real WAN origin, TLS, or an origin that is slower than real time (which can never catch up and
  fails the seek after 60 s by design).
- Memory beyond the structural bound: the preparation owns one decoder and a few frames of PCM, and
  the cleanup tests show both are released after a swap, a failure, a newer seek, a stop and a
  dropped session.
