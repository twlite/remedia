#!/usr/bin/env python3
"""Measure bounded plugin-media transfer queue and thread strategies.

Build with:
    cargo build --release --locked -p remedia-server --example plugin_media_threads

Each run is a fresh process. The harness is synthetic: it measures process RSS/footprint,
OS-thread count, queue occupancy and cleanup, not network, FFmpeg, or native heap behavior.
"""

import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = ROOT / "target/release/examples/plugin_media_threads"
MODES = ["dedicated-blocked", "pool-blocked", "dedicated-idle", "async-idle"]


def run_one(binary, mode, streams, buffer_bytes, timeout):
    command = [str(binary), mode, str(streams), str(buffer_bytes)]

    try:
        completed = subprocess.run(
            command, capture_output=True, text=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired:
        return None, f"timed out after {timeout}s"

    if completed.returncode != 0:
        return None, f"exit {completed.returncode}: {completed.stderr.strip()[:300]}"

    try:
        return json.loads(completed.stdout.strip().splitlines()[-1]), None
    except (IndexError, json.JSONDecodeError) as error:
        return None, f"unparseable output: {error}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=str(DEFAULT_BINARY))
    parser.add_argument("--out", required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--streams", type=int, nargs="+", default=[8, 32, 128])
    parser.add_argument("--buffers", type=int, nargs="+", default=[4096, 262144])
    parser.add_argument("--modes", nargs="+", choices=MODES, default=MODES)
    parser.add_argument("--timeout", type=float, default=15.0)
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        sys.exit(f"missing {binary}; build the example first")
    if args.repeats < 3:
        sys.exit("use at least three repetitions")
    if any(streams < 1 for streams in args.streams):
        sys.exit("stream counts must be positive")
    if any(buffer_bytes < 1 for buffer_bytes in args.buffers):
        sys.exit("buffer sizes must be positive")

    cases = [
        (mode, streams, buffer_bytes)
        for streams in args.streams
        for buffer_bytes in args.buffers
        for mode in args.modes
    ]
    rows = []
    failures = 0

    with open(args.out, "w") as output:
        for repeat in range(1, args.repeats + 1):
            offset = (repeat - 1) % len(cases)
            ordered_cases = cases[offset:] + cases[:offset]

            for mode, streams, buffer_bytes in ordered_cases:
                row, error = run_one(
                    binary, mode, streams, buffer_bytes, args.timeout
                )
                if row is None:
                    failures += 1
                    row = {
                        "mode": mode,
                        "streams": streams,
                        "buffer_bytes": buffer_bytes,
                        "repeat": repeat,
                        "error": error,
                    }
                    print(
                        f"FAILED {mode} n={streams} buf={buffer_bytes}: {error}",
                        file=sys.stderr,
                    )
                else:
                    row["repeat"] = repeat
                    expected_sentinel = mode != "pool-blocked"
                    if row["pending_bytes_after_release"] != 0:
                        failures += 1
                        error = "queued bytes remained after consumer release"
                    elif row["open_writers_after_release"] != 0:
                        failures += 1
                        error = "writers remained open after consumer release"
                    elif row["cleanup_threads_delta"] != 0:
                        failures += 1
                        error = "transfer or runtime threads remained after bounded cleanup wait"
                    elif row["sentinel_ready_before_release"] != expected_sentinel:
                        failures += 1
                        error = "blocking-pool sentinel result did not match the mode"
                    else:
                        error = None

                    expected_full_queues = (
                        streams if mode == "dedicated-blocked" else
                        min(streams, row["blocking_pool_threads"]) if mode == "pool-blocked" else
                        0
                    )
                    expected_pending_bytes = expected_full_queues * buffer_bytes
                    if error is None and (
                        row["full_queues"] != expected_full_queues
                        or row["pending_bytes"] != expected_pending_bytes
                    ):
                        failures += 1
                        error = "measured queue occupancy differed from the blocked-producer setup"

                    if error:
                        row["verification_error"] = error
                        print(
                            f"FAILED {mode} n={streams} buf={buffer_bytes}: {error}",
                            file=sys.stderr,
                        )
                    else:
                        print(
                            f"{mode:18} n={streams:4} buf={buffer_bytes:7} "
                            f"threads+{row['threads_delta']} "
                            f"rss_delta={row['rss_delta_bytes']} "
                            f"queued={row['pending_bytes']} "
                            f"sentinel={row['sentinel_ready_before_release']} "
                            f"cleanup_threads+{row['cleanup_threads_delta']}"
                        )

                output.write(json.dumps(row) + "\n")
                output.flush()
                rows.append(row)

    for case in cases:
        matching = [
            row
            for row in rows
            if (row.get("mode"), row.get("streams"), row.get("buffer_bytes"))
            == case
            and "error" not in row
            and "verification_error" not in row
        ]
        if len(matching) != args.repeats:
            continue

        rss_values = [row["rss_delta_bytes"] for row in matching]
        thread_values = [row["threads_delta"] for row in matching]
        pending_values = [row["pending_bytes"] for row in matching]
        full_queue_values = [row["full_queues"] for row in matching]
        print(
            f"median {case[0]:18} n={case[1]:4} buf={case[2]:7}: "
            f"RSS delta {statistics.median(rss_values):.0f} B "
            f"(range {min(rss_values)}..{max(rss_values)}); "
            f"threads +{statistics.median(thread_values):.0f} "
            f"(range {min(thread_values)}..{max(thread_values)}); "
            f"queue {statistics.median(pending_values):.0f} B, "
            f"{statistics.median(full_queue_values):.0f} full queues; cleanup threads +0"
        )

    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
