"""Produce a complete median comparison table from the committed JSONL runs."""
import collections
import json
import statistics
from pathlib import Path


def grouped(path):
    result = collections.defaultdict(list)
    for line in Path(path).read_text().splitlines():
        row = json.loads(line)
        key = (row['fixture'], row['workload'], row['pacing'], row['sessions'])
        result[key].append(row)
    return result


before = grouped('benchmarks/baseline.jsonl')
after = grouped('benchmarks/after.jsonl')
columns = ['wall_s', 'child_cpu_s', 'peak_sampled_rss_kib', 'peak_sampled_threads',
           'startup_p99_us', 'sink_delay_p99_us', 'packet_capacity_bytes']
lines = ['# Recorded benchmark medians', '',
         'Each cell is baseline → changed. RSS and thread peaks are sampled lower bounds.', '',
         '| Fixture / workload / pacing / sessions | Runs | Wall s | CPU s | RSS KiB | Threads | Startup p99 µs | Sink delay p99 µs | Cumulative packet capacity B |',
         '| --- | --- | --- | --- | --- | --- | --- | --- | --- |']
for key in before:
    if key not in after:
        raise ValueError(f'missing changed workload: {key}')
    old, new = before[key], after[key]
    if len(old) != 3 or len(new) != 3:
        raise ValueError(f'expected three repetitions: {key}')
    for left, right in zip(old, new):
        assert left['packets'] == right['packets'], key
        assert left['audio_s'] == right['audio_s'], key
    cells = []
    for name in columns:
        values = [statistics.median(row[name] for row in group) for group in (old, new)]
        cells.append(' → '.join(f'{v:.3f}' if name.endswith('_s') else str(v) for v in values))
    lines.append('| ' + ' / '.join(map(str, key)) + ' | 3 / 3 | ' + ' | '.join(cells) + ' |')
Path('benchmarks/results.md').write_text('\n'.join(lines) + '\n')
