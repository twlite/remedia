#!/usr/bin/env python3
"""Run paired blocking/async completion waiter resource measurements."""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
from pathlib import Path
from typing import Any


COUNTS = (1, 8, 32, 64, 128, 256, 512)


def parse_counts(value: str) -> tuple[int, ...]:
    try:
        counts = tuple(int(part) for part in value.split(","))
    except ValueError as error:
        raise argparse.ArgumentTypeError("counts must be comma-separated integers") from error

    if not counts or any(count <= 0 for count in counts):
        raise argparse.ArgumentTypeError("counts must all be positive")

    return counts


def run_case(binary: Path, mode: str, count: int, alloc: bool) -> dict[str, Any]:
    command = [str(binary), mode, str(count)]
    if alloc:
        command.append("alloc")

    completed = subprocess.run(
        command,
        capture_output=True,
        check=False,
        text=True,
        timeout=60,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"{command!r} exited {completed.returncode}: {completed.stderr.strip()}"
        )

    result = json.loads(completed.stdout)
    if result["mode"] != mode or result["waiters"] != count:
        raise RuntimeError(f"benchmark returned unexpected identity: {result!r}")
    if result["completedWaiters"] != count or not result["outputFinished"]:
        raise RuntimeError(f"benchmark did not complete all waiters: {result!r}")
    if result["sampleFramesPerChannel"] != 960:
        raise RuntimeError(f"PCM sample count changed: {result!r}")
    if mode == "async" and result["retainedTaskWakersAfterCompletion"] != 0:
        raise RuntimeError(f"async completion retained wakers: {result!r}")

    return result


def display_summary(records: list[dict[str, Any]]) -> None:
    print(
        "mode alloc waiters reps threads-pending median-completion-us "
        "completion-range-us peak-rss-MiB rust-live-pending-KiB"
    )
    keys = sorted(
        {(row["mode"], row["alloc"], row["waiters"]) for row in records},
        key=lambda key: (key[1], key[2], key[0]),
    )
    for mode, alloc, count in keys:
        rows = [
            row["result"]
            for row in records
            if row["mode"] == mode and row["alloc"] == alloc and row["waiters"] == count
        ]
        completions = [float(row["completionUs"]) for row in rows]
        threads = [row["threadCountPending"] for row in rows]
        rss = [row["peakResidentBytes"] for row in rows]
        heap = [row["rustHeapLivePendingBytes"] for row in rows]
        median_completion = statistics.median(completions)
        completion_range = max(completions) - min(completions)
        median_rss = statistics.median(value for value in rss if value is not None)
        median_heap = statistics.median(value for value in heap if value is not None) if alloc else None
        print(
            f"{mode:8} {str(alloc):5} {count:7} {len(rows):4} "
            f"{statistics.median(value for value in threads if value is not None):14.1f} "
            f"{median_completion:22.1f} {completion_range:19.1f} "
            f"{median_rss / (1024 * 1024):13.2f} "
            f"{(median_heap / 1024):23.1f}" if alloc else
            f"{mode:8} {str(alloc):5} {count:7} {len(rows):4} "
            f"{statistics.median(value for value in threads if value is not None):14.1f} "
            f"{median_completion:22.1f} {completion_range:19.1f} "
            f"{median_rss / (1024 * 1024):13.2f} {'n/a':>23}"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="release benchmark executable")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).with_name("resource-audit-1.jsonl"),
        help="raw JSONL destination",
    )
    parser.add_argument("--counts", type=parse_counts, default=COUNTS)
    parser.add_argument("--repeats", type=int, default=5, help="un-instrumented repetitions")
    parser.add_argument("--alloc-repeats", type=int, default=3, help="Rust heap counter repetitions")

    arguments = parser.parse_args()
    binary = arguments.binary.resolve()
    if not binary.is_file():
        parser.error(f"benchmark binary does not exist: {binary}")
    if arguments.repeats < 1 or arguments.alloc_repeats < 1:
        parser.error("repeat counts must be positive")

    records: list[dict[str, Any]] = []
    for alloc, repeats in ((False, arguments.repeats), (True, arguments.alloc_repeats)):
        for count in arguments.counts:
            for repeat in range(repeats):
                modes = ("blocking", "async") if repeat % 2 == 0 else ("async", "blocking")
                paired: dict[str, dict[str, Any]] = {}
                for mode in modes:
                    result = run_case(binary, mode, count, alloc)
                    paired[mode] = result
                    records.append(
                        {
                            "mode": mode,
                            "waiters": count,
                            "repeat": repeat + 1,
                            "alloc": alloc,
                            "result": result,
                        }
                    )

                for field in ("outputFrames", "sampleFramesPerChannel", "outputFinished"):
                    if paired["blocking"][field] != paired["async"][field]:
                        raise RuntimeError(
                            f"paired correctness mismatch at {count} waiters, repeat {repeat + 1}: {field}"
                        )

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        "".join(json.dumps(row, sort_keys=True) + "\n" for row in records),
        encoding="utf-8",
    )
    print(f"raw results: {arguments.output.resolve()}")
    display_summary(records)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
