"""Compare two resource_matrix.py outputs (before -> after) as a Markdown table.

Usage: summarize_resources.py BEFORE.jsonl AFTER.jsonl [--title TEXT]

Cells are medians over repetitions. CPU is the whole child process (kernel `wait4` user+system
seconds, including startup and teardown); steady CPU excludes the first three seconds. RSS is the
kernel's peak resident size for that process (`ru_maxrss`), footprint the largest one-second
sample of macOS physical footprint (0 elsewhere). Heap-counter rows (`alloc: true`) are summarized
separately and are never mixed into CPU or RSS columns.
"""
import argparse
import collections
import json
import statistics
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('before')
parser.add_argument('after')
parser.add_argument('--title', default='Resource comparison')
args = parser.parse_args()


def load(path):
    groups = collections.defaultdict(list)
    for line in Path(path).read_text().splitlines():
        row = json.loads(line)
        groups[(row['case'], row['sessions'], row.get('alloc', False))].append(row)
    return groups


def median(rows, name):
    values = [row[name] for row in rows if row.get(name) is not None]
    return statistics.median(values) if values else None


def cell(before, after, name, scale=1.0, digits=2):
    left, right = median(before, name), median(after, name)
    if left is None or right is None:
        return 'n/a'
    left, right = left / scale, right / scale
    delta = f' ({(right - left) / left * 100:+.0f}%)' if left else ''
    return f'{left:.{digits}f} → {right:.{digits}f}{delta}'


before_rows, after_rows = load(args.before), load(args.after)
MIB = 1048576
print(f'# {args.title}\n')
print('Each cell is before → after (median of repetitions; % is the change).\n')
print('| Case | Sessions | Reps | Steady CPU s | Total CPU s | Peak RSS MiB | Footprint MiB | Threads | Passthrough share | Sink p99 µs |')
print('| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |')
for key in sorted(before_rows, key=lambda k: (list(before_rows).index(k))):
    case, sessions, alloc = key
    if alloc or key not in after_rows:
        continue
    before, after = before_rows[key], after_rows[key]
    share = ' → '.join(
        'n/a' if median(rows, 'passthrough_fraction') is None else f"{median(rows, 'passthrough_fraction'):.2f}"
        for rows in (before, after))
    print(f'| {case} | {sessions} | {len(before)}/{len(after)} | '
          f"{cell(before, after, 'steady_cpu_s')} | {cell(before, after, 'cpu_s')} | "
          f"{cell(before, after, 'peak_rss_bytes', MIB, 1)} | {cell(before, after, 'max_footprint_bytes', MIB, 1)} | "
          f"{cell(before, after, 'max_threads', 1, 0)} | {share} | {cell(before, after, 'sink_delay_p99_us', 1, 0)} |")

heap = [key for key in before_rows if key[2] and key in after_rows]
if heap:
    print('\n## Heap counters (Rust allocator only; FFmpeg and libopus allocate natively and are not counted)\n')
    print('| Case | Sessions | Allocations / audio s | Bytes allocated / audio s | Peak live heap KiB |')
    print('| --- | --- | --- | --- | --- |')
    for key in heap:
        case, sessions, _ = key
        cells = []
        for metric in ('allocations', 'allocated_bytes', 'peak_live_bytes'):
            pair = []
            for rows in (before_rows[key], after_rows[key]):
                values = [row['heap'][metric] / (row['audio_s'] if metric != 'peak_live_bytes' else 1024) for row in rows if row.get('heap')]
                pair.append(statistics.median(values))
            cells.append(f'{pair[0]:.0f} → {pair[1]:.0f} ({(pair[1] - pair[0]) / pair[0] * 100:+.0f}%)')
        print(f'| {case} | {sessions} | ' + ' | '.join(cells) + ' |')
