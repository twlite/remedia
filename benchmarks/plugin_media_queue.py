#!/usr/bin/env python3
"""Measure plugin-like byte-stream queue ingestion and decoded sample integrity.

Build with:
    cargo build --release -p remedia-core --example plugin_media_queue

Each sample runs in a fresh process. Mode order alternates between repetitions, and every mode
must match an in-memory decode's sample count, decoded frame count, and checksum. Use a separate
build with --features bench-allocation-metrics to count Rust allocation requests during queue
writes; those counts exclude response-chunk creation, decoder allocations, native FFmpeg
allocations, retained bytes, and process RSS.
"""

import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = ROOT / "target/release/examples/plugin_media_queue"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=str(DEFAULT_BINARY))
    parser.add_argument("--out", required=True)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--media-bytes", type=int, default=8388608)
    parser.add_argument("--chunk-bytes", type=int, nargs="+", default=[8192, 16384])
    parser.add_argument("--modes", nargs="+", choices=["copy", "bytes"], default=["copy", "bytes"])
    parser.add_argument("--allocation-metrics", action="store_true")
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        sys.exit(f"missing {binary}; build the example first")
    if args.repeats < 3:
        sys.exit("use at least three repetitions")
    if len(args.modes) not in (1, 2):
        sys.exit("run one mode for a baseline or both modes for an A/B comparison")

    rows = []
    with open(args.out, "w") as output:
        for chunk_bytes in args.chunk_bytes:
            mode_checksums = {mode: set() for mode in args.modes}
            mode_samples = {mode: set() for mode in args.modes}
            mode_frames = {mode: set() for mode in args.modes}
            for repeat in range(args.repeats):
                modes = list(args.modes)
                if repeat % 2:
                    modes.reverse()

                for mode in modes:
                    completed = subprocess.run(
                        [str(binary), mode, str(chunk_bytes), str(args.media_bytes)],
                        capture_output=True,
                        text=True,
                        timeout=60,
                        check=False,
                    )
                    if completed.returncode != 0:
                        sys.exit(f"{mode} run failed: {completed.stderr.strip()}")

                    row = json.loads(completed.stdout.strip().splitlines()[-1])
                    if row["allocation_metrics_enabled"] != args.allocation_metrics:
                        sys.exit("binary allocation-metrics feature does not match runner option")
                    if row["media_bytes"] != args.media_bytes:
                        sys.exit(f"binary processed an unexpected fixture size: {row}")
                    if row["samples_per_channel"] != args.media_bytes // 2:
                        sys.exit(f"decoded sample count differs from the generated PCM fixture: {row}")
                    if row["max_pending_bytes"] > row["queue_capacity_bytes"]:
                        sys.exit(f"queue exceeded its configured byte capacity: {row}")
                    row["repeat"] = repeat + 1
                    rows.append(row)
                    output.write(json.dumps(row) + "\n")
                    output.flush()
                    mode_checksums[mode].add(row["checksum"])
                    mode_samples[mode].add(row["samples_per_channel"])
                    mode_frames[mode].add(row["decoded_frames"])

            for values, label in (
                (mode_checksums, "checksum"),
                (mode_samples, "sample count"),
                (mode_frames, "decoded frame count"),
            ):
                if len({tuple(sorted(items)) for items in values.values()}) != 1:
                    sys.exit(f"modes differ in {label} for {chunk_bytes}-byte chunks")
                if any(len(items) != 1 for items in values.values()):
                    sys.exit(f"repetitions differ in {label} for {chunk_bytes}-byte chunks")

            for mode in args.modes:
                samples = [
                    row["mib_per_second"]
                    for row in rows
                    if row["chunk_bytes"] == chunk_bytes and row["mode"] == mode
                ]
                median = statistics.median(samples)
                low, high = min(samples), max(samples)
                matching = [
                    row
                    for row in rows
                    if row["chunk_bytes"] == chunk_bytes and row["mode"] == mode
                ]
                decoded_frames = matching[0]["decoded_frames"]
                print(
                    f"{mode:5} chunk={chunk_bytes:5} bytes "
                    f"median={median:8.1f} MiB/s range={low:.1f}..{high:.1f}; "
                    f"samples={matching[0]['samples_per_channel']} frames={decoded_frames} "
                    f"checksum={matching[0]['checksum']}"
                )

                if args.allocation_metrics:
                    allocated = [row["write_allocation_bytes"] for row in matching]
                    allocation_counts = [row["write_allocation_count"] for row in matching]
                    print(
                        f"      queue-write allocations median={statistics.median(allocated):.0f} B "
                        f"({statistics.median(allocation_counts):.0f} requests)"
                    )


if __name__ == "__main__":
    main()
