# Resource audit 2: staged PCM allocation and stall behavior

## Reproduction and scope

The ignored release test compares eager full-capacity reservation with lazy reservation using the
same `StagedPcmBuffer` code, real-time producer/consumer threads, and generated stereo sample ramps.
It runs 128 independent buffers for five repetitions at 250, 500, 1,000 and 2,000 ms. Each trial
starts with a full ring, pauses its producer for the same duration as the ring capacity, then
resumes with paced 20 ms blocks at real-time pace. The source length is aligned to whole output
blocks, including a final partial producer write for the 250 ms case. Every consumed sample pair is
checked against the monotonic ramp. The harness drains and verifies any remaining staged frames
after the real-time window, so an output underrun is distinguished from dropped or reordered media
samples.

The eager policy reserves the whole ring on construction, matching the prior constructor behavior.
The lazy policy is the implementation now used: ring storage grows in 500 ms chunks up to the same
configured capacity. The production staging cap remains two seconds.

```sh
set -o pipefail
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig \
RUSTFLAGS='-L native=/opt/homebrew/opt/lame/lib -L native=/opt/homebrew/opt/svt-av1/lib -L native=/opt/homebrew/opt/openssl@4/lib -l c++' \
  cargo test --release -p remedia-core --locked \
  resource_audit_staging_realtime_stalls -- --ignored --nocapture \
  | rg '^\{' > benchmarks/resource-audit-2-staging-realtime.jsonl
```

This is a ring-buffer benchmark, not an FFmpeg decode or HTTP integration run. It uses Cargo's
locked release profile and native link environment from the engine benchmarks, but generates
deterministic PCM ramps in memory instead of opening a media fixture. `bytesAtCreation` and
`bytesAtStall` are Rust `VecDeque` capacities multiplied by `sizeof(f32)` across 128 buffers;
they measure retained Rust ring capacity, not native heap, allocator resident pages, or process RSS.
`deliveredFrames` counts only frames consumed on the real-time schedule; `sampleFramesVerified`
includes the final drain and must equal the full generated source. Timing includes thread setup,
the programmed stall, paced production, consumption and final drain.

## Measurements

Each line in [the raw JSONL](resource-audit-2-staging-realtime.jsonl) is one repeat. Memory values
are medians; underflows and delivered frames are totals across all 128 streams. Elapsed ranges are
the minimum and maximum of five repeats. A continuity value of `true` means all delivered samples
remained in order with no media-sample gaps.

| Ring and producer stall | Eager bytes at creation | Lazy bytes at creation | Bytes at full staging, either policy | Underrun blocks, eager / lazy (median and range) |              Realtime frames delivered, eager / lazy (median and range) | Elapsed median eager (range), µs | Elapsed median lazy (range), µs |
| ----------------------- | ----------------------: | ---------------------: | -----------------------------------: | -----------------------------------------------: | ----------------------------------------------------------------------: | -------------------------------: | ------------------------------: |
| 250 ms                  |              12,288,000 |                      0 |                           12,288,000 |                    230 (216–256) / 236 (189–413) |       7,643,520 (7,618,560–7,656,960) / 7,637,760 (7,467,840–7,682,880) |  1,293,951 (1,291,992–1,296,853) | 1,294,748 (1,293,353–1,297,974) |
| 500 ms                  |              24,576,000 |                      0 |                           24,576,000 |                    128 (128–128) / 128 (128–128) |       9,216,000 (9,216,000–9,216,000) / 9,216,000 (9,216,000–9,216,000) |  1,537,328 (1,531,391–1,541,228) | 1,538,210 (1,532,879–1,539,166) |
| 1,000 ms                |              49,152,000 |                      0 |                           49,152,000 |                    128 (128–128) / 128 (128–488) | 12,288,000 (12,288,000–12,288,000) / 12,288,000 (11,942,400–12,288,000) |  2,032,405 (2,032,068–2,035,161) | 2,036,735 (2,034,467–2,049,696) |
| 2,000 ms                |              98,304,000 |                      0 |                           98,304,000 |                    128 (128–128) / 128 (128–128) | 18,432,000 (18,432,000–18,432,000) / 18,432,000 (18,432,000–18,432,000) |  3,039,342 (3,035,115–3,043,220) | 3,038,911 (3,038,104–3,046,300) |

Every raw record reports the same fully verified frame count for both strategies at each capacity:
7,741,440 at 250 ms, 9,216,000 at 500 ms, 12,288,000 at 1,000 ms and 18,432,000 at 2,000 ms.
`underrunBlocks` counts missed real-time output requests across 128 streams. All runs include the
128 requests at the exact stall boundary; the additional spread at 250 ms and one 1,000 ms lazy run
shows scheduler noise under 256 competing worker threads. The final drain confirms those misses
did not discard or reorder staged samples. Lazy allocation removes the initial ring reservation for
all 128 buffers. Once fully staged, both policies reserve the same amount; timing ranges overlap
and do not establish a throughput improvement. These results support keeping the two-second cap and
do not justify reducing stall tolerance.

## Resource ownership after promotion

When a promoted local Opus file or byte-buffer decoder is handed from the staging worker to the
playback worker, all staged samples have already been consumed. The ring allocation is released at
that handoff. Cancelling a stage also drops its PCM storage after waking blocked producers. Remote
and producer-fed stages keep their bounded PCM cushion because the decoder remains on its producer
thread and can stall after promotion.
