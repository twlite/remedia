# Plugin media transfer and queue audit

## Findings

`ByteStreamWriter::write_bytes(Bytes)` removes the server's `Bytes::to_vec()` allocation for a
response chunk whose visible length fits the configured queue. It does not make the queue's byte
limit a strict memory limit. A fitting `write_owned(Vec<u8>)` chunk has always been queued based on
its length while retaining any excess vector capacity; the new shared-bytes path can retain the
backing allocation behind a short slice. The core API Rustdoc and [decoding guide](/docs/core/decoding#in-memory-bytes-and-producer-streams)
now state both cases and recommend right-sizing a small slice when its parent is much larger.

In the checked-in lockfile, reqwest is `0.13.5` and Hyper is `1.12.0`. `reqwest::Response::chunk()`
returns `Bytes` from the response body's DATA frame (`frame.into_data()`); it does not copy the
frame into a new `Vec`. Hyper's HTTP/1 client uses a read buffer with an 8 KiB initial size and a
417,792-byte logical maximum. Reqwest leaves Hyper's exact read-buffer size unset, so that adaptive
strategy applies. Hyper's body reader returns `split_to(min(requested, available)).freeze()` from
the read buffer. HTTP/2 delivers DATA-frame bytes. Neither path exposes the retained backing
allocation through `Bytes`, and the HTTP/1 logical buffer limit is not a public backing-capacity
contract. Therefore the server
does not guess from visible chunk length and make a compensating copy. It passes the returned
`Bytes` directly to the bounded writer. The pump does not request another body chunk until the
current `write_bytes` call is accepted, so it has at most one current response chunk waiting on
queue backpressure per transfer. `mediaBufferBytes` covers visible queued bytes; HTTP client,
decoder, and shared backing allocations are outside that figure. Existing URL, redirect, address,
response-length, and total-media checks are unchanged.

After dequeue, the core reader copies bytes into FFmpeg's input buffer; this change removes the
server-to-queue `Vec` copy only. The transfer thread owns the reqwest response while it waits on
the synchronous queue writer and does not fetch the next body chunk until backpressure clears.

## Queue-copy and decoder measurements

The queue benchmark creates a deterministic 8 MiB mono, 48 kHz PCM WAV payload and feeds it to the
same FFmpeg decoder in both modes. For each chunk size, `copy` uses the prior
`write_owned(response_chunk.to_vec())` path and `bytes` uses `write_bytes(response_chunk)`. The
benchmark checks both paths against a separate in-memory decode and records maximum pending queue
bytes. The checked-in unmetered run has 15 fresh-process repetitions per mode and chunk size.

|  Chunk | Mode  |    Throughput median (range) | Peak pending bytes |
| -----: | ----- | ---------------------------: | -----------------: |
|  8 KiB | copy  | 296.56 MiB/s (289.06–301.42) |            262,144 |
|  8 KiB | Bytes | 297.80 MiB/s (291.83–304.05) |            262,144 |
| 16 KiB | copy  | 295.81 MiB/s (289.76–300.61) |            262,144 |
| 16 KiB | Bytes | 294.93 MiB/s (291.43–298.71) |            262,144 |

All 60 runs decoded 4,194,304 samples per channel into 1,024 frames and produced checksum
`9707069788100184977`. Both modes reached the 256 KiB queue capacity. The timing ranges overlap;
these data do not show a throughput gain.

A separate build with the opt-in `bench-allocation-metrics` feature measured Rust allocator
requests made only by the producer during each queue write. It used the same release profile,
lockfile, synthetic fixture, and chunk sizes, with five fresh-process repetitions per mode and
size.

|  Chunk | Mode  | Allocated bytes / requests per 8 MiB run |
| -----: | ----- | ---------------------------------------: |
|  8 KiB | copy  |                      8,390,400 B / 1,027 |
|  8 KiB | Bytes |                              1,792 B / 3 |
| 16 KiB | copy  |                        8,389,376 B / 514 |
| 16 KiB | Bytes |                                768 B / 2 |

Each pair differs by exactly 8,388,608 allocated bytes, the 8 MiB payload passed through
`to_vec()` in the old path. The allocation counter excludes response-chunk construction, decoder
allocations, FFmpeg/native allocations, retained backing capacity, and process RSS. The pointer
retention unit test separately verifies that a four-byte `Bytes` slice keeps its 1 MiB parent's
storage alive until the queued data is read. These measurements count allocation requests; they do
not measure native heap use or prove an RSS/leak fix.

## Transfer threads, queue occupancy, and cleanup

`plugin_media_threads` is a synthetic bounded-queue harness, not an HTTP or FFmpeg benchmark. It
compares the production dedicated blocked writer, a blocked `spawn_blocking` pool with 16 workers,
and idle dedicated-thread/task resource bounds. It filled real `ByteStreamWriter` queues with
stalled readers and verified pending bytes and full-queue counts separately from process RSS. The
runner used three fresh processes for each stream count and buffer size: 8, 32, and 128 streams;
4 KiB and 256 KiB per stream. RSS deltas below are medians and ranges from those three runs. The
thread delta is stable across all three repetitions. The harness uses a two-worker Tokio runtime
and caps its blocking pool at 16 threads; the production host defaults to 8 active streams and
256 KiB per queue, with a configurable 1,024-stream maximum.

| Dedicated blocked writers | Queued bytes | OS threads added |             RSS delta median (range) |
| ------------------------- | -----------: | ---------------: | -----------------------------------: |
| 8 × 4 KiB                 |     32,768 B |                9 |          245,760 B (245,760–311,296) |
| 8 × 256 KiB               |  2,097,152 B |                9 |    2,457,600 B (2,441,216–2,490,368) |
| 32 × 4 KiB                |    131,072 B |               33 |        999,424 B (983,040–1,015,808) |
| 32 × 256 KiB              |  8,388,608 B |               33 |    9,666,560 B (9,666,560–9,682,944) |
| 128 × 4 KiB               |    524,288 B |              129 |    3,506,176 B (3,506,176–3,588,096) |
| 128 × 256 KiB             | 33,554,432 B |              129 | 38,338,560 B (38,322,176–38,338,560) |

Smaller queues reduce the stalled-reader queue occupancy by 64 times. The synthetic test does not
measure decoder throughput, HTTP chunking, or the effect of increasing producer wakeups; no default
buffer size was changed. The dedicated path used one transfer thread per stream plus the observed
runtime thread. Releasing the byte-stream consumers drained every queue, closed every writer, and
returned the thread count to its initial baseline in all 72 runs. The post-release RSS delta from
the process's initial snapshot still reached 36,503,552 bytes for 128 × 256 KiB. Queue occupancy,
thread count, and RSS are different observations: the harness does not inventory all retained Rust
objects or measure native heap allocations, and residual RSS alone is not evidence of live queued
bytes or a leak.

The bounded-pool alternative kept the 128-stream harness to 16 blocking threads, but only 16
streams filled their queues (65,536 B at 4 KiB each or 4,194,304 B at 256 KiB each); the other
blocking tasks waited in the pool. An unrelated `spawn_blocking` sentinel did not start within the
100 ms probe in any pool run (observed wait 101.2–102.1 ms). The dedicated-thread runs let the same
sentinel start before readers were released. This demonstrates pool starvation for a scheduler
alternative that blocks those workers. The async-idle harness used one Tokio thread for 128 idle
tasks, but those tasks only waited for stream closure; it did not implement async queue writes or
HTTP/decoder ownership and is not an equivalent replacement candidate. These measurements support
keeping the current dedicated transfer loop until a bounded, cancellable async handoff is measured
end to end. They do not qualify the configured 1,024-stream upper limit; this matrix stopped at 128.

## Lifecycle regression coverage

- Core tests repeat 2,048 blocked-producer cycles, alternating explicit cancellation and reader
  drop, and repeat 1,024 blocked-reader cancellation cycles.
- Server integration tests run real loopback HTTP plugin-media transfers. Repeated stop cycles
  assert active-stream slots return to zero within two seconds and observe upstream disconnects.
  A stalled response times out and releases its upstream and stream slot; a truncated/disconnected
  response continues to fail and clean up.
- The server still uses its per-host active-stream semaphore, transfer cancellation token, idle
  timeout, body-length validation, maximum-media-byte limit, and SSRF/redirect policy.

The benchmark does not measure a distribution of live reqwest chunk lengths or hidden backing
capacities. The Hyper source review identifies the current HTTP/1 read-buffer behavior, but it
cannot turn that implementation detail into a stable memory guarantee; `Bytes` exposes no
capacity accessor. HTTP/2 backing allocations are likewise not measured here.

The decoder benchmark checks sample count, decoded frame count, and checksum. It does not produce
RTP packets, so packet totals are not applicable to this byte-ingress test. The independent
original-main codec baseline's packet/sample totals are documented in
[resource-audit-integration.md](resource-audit-integration.md).

## Reproduction

Measurements were made on Apple M5 arm64, macOS 27.0.1, Rust/Cargo 1.99.1, release profile, the
checked-in `Cargo.lock`, and native FFmpeg from the Homebrew pkg-config directory. The linker emits
a non-fatal QTKit warning because the installed framework has no arm64 slice.

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig
export RUSTFLAGS='-L native=/opt/homebrew/opt/lame/lib -L native=/opt/homebrew/opt/svt-av1/lib -L native=/opt/homebrew/opt/openssl@4/lib -l c++'

cargo build --release --locked -p remedia-core --example plugin_media_queue
cp target/release/examples/plugin_media_queue /tmp/remedia-plugin-media-queue-unmetered
python3 benchmarks/plugin_media_queue.py \
  --binary /tmp/remedia-plugin-media-queue-unmetered \
  --out benchmarks/data/plugin-media-queue-time.jsonl \
  --repeats 15 --media-bytes 8388608 --chunk-bytes 8192 16384

cargo build --release --locked -p remedia-core --example plugin_media_queue \
  --features bench-allocation-metrics
cp target/release/examples/plugin_media_queue /tmp/remedia-plugin-media-queue-allocations
python3 benchmarks/plugin_media_queue.py \
  --binary /tmp/remedia-plugin-media-queue-allocations \
  --out benchmarks/data/plugin-media-queue-allocations.jsonl \
  --repeats 5 --media-bytes 8388608 --chunk-bytes 8192 16384 \
  --allocation-metrics

cargo build --release --locked -p remedia-server --example plugin_media_threads
python3 benchmarks/plugin_media_threads.py \
  --out benchmarks/data/plugin-media-threads-audit.jsonl \
  --repeats 3 --streams 8 32 128 --buffers 4096 262144
```

Raw JSONL is checked in at [`data/plugin-media-queue-time.jsonl`](data/plugin-media-queue-time.jsonl),
[`data/plugin-media-queue-allocations.jsonl`](data/plugin-media-queue-allocations.jsonl), and
[`data/plugin-media-threads-audit.jsonl`](data/plugin-media-threads-audit.jsonl). The interrupted
first-pass files `resource-audit-4-plugin-media-queue-before.jsonl`,
`resource-audit-4-plugin-media-queue-after.jsonl`, and `resource-audit-4-plugin-media-threads-before.jsonl`
have uneven repetitions and incompatible metrics; they are retained as historical input only and
are not used for the comparisons above.
