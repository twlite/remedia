#!/usr/bin/env python3
"""Run the plugin media transfer thread-cost matrix with a hard per-run timeout.

Every configuration runs in a fresh process (so resident memory is attributable). A run that
exceeds --timeout seconds is killed and recorded as a failure instead of blocking the matrix.

    cargo build --release -p remedia-server --example plugin_media_threads
    python3 benchmarks/plugin_media_threads.py --out benchmarks/data/plugin-media-threads.jsonl
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/release/examples/plugin_media_threads"
CONFIGS = [("blocked", 262144), ("blocked", 4096), ("idle", 262144), ("tasks", 262144)]


def run_one(mode, streams, buffer_bytes, timeout):
    command = [str(BINARY), mode, str(streams), str(buffer_bytes)]
    try:
        completed = subprocess.run(
            command, capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        return None, f"timed out after {timeout}s"
    if completed.returncode != 0:
        return None, f"exit {completed.returncode}: {completed.stderr.strip()[:200]}"
    try:
        return json.loads(completed.stdout.strip().splitlines()[-1]), None
    except (IndexError, json.JSONDecodeError) as error:
        return None, f"unparseable output: {error}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--streams", type=int, nargs="+", default=[1, 32, 128, 512, 1024])
    parser.add_argument("--timeout", type=float, default=60.0)
    args = parser.parse_args()

    if not BINARY.exists():
        sys.exit(f"missing {BINARY}; build the example first")

    failures = 0
    with open(args.out, "w") as out:
        for streams in args.streams:
            for mode, buffer_bytes in CONFIGS:
                result, error = run_one(mode, streams, buffer_bytes, args.timeout)
                if result is None:
                    failures += 1
                    result = {
                        "mode": mode,
                        "streams": streams,
                        "buffer_bytes": buffer_bytes,
                        "error": error,
                    }
                    print(f"FAILED {mode} n={streams} buf={buffer_bytes}: {error}", file=sys.stderr)
                else:
                    print(
                        f"{mode:8} n={streams:5} buf={buffer_bytes:7} "
                        f"per-stream={result['rss_per_stream_kib']:7.1f} KiB "
                        f"threads+{result['threads_delta']} cpu={result['cpu_percent']:.2f}%"
                    )
                out.write(json.dumps(result) + "\n")
                out.flush()
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
