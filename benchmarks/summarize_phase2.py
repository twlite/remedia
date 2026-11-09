"""Regenerate the phase 2 measurement tables; no external services."""
import hashlib
import json
import statistics
from pathlib import Path

before = [json.loads(line) for line in Path('benchmarks/phase2-baseline.jsonl').read_text().splitlines()]
after = [json.loads(line) for line in Path('benchmarks/phase2-after.jsonl').read_text().splitlines()]
lines = ['# Phase 2 recorded measurements', '',
         'Three-run medians, paced eligible Opus with crossfade policy enabled and no staged track. CPU is process user + system seconds; these are short 2.02-second fixtures. Each cell is phase 1 → phase 2.', '',
         '| Sessions | CPU seconds | RSS KiB | OS threads | Startup p99 µs | Sink delay p95 µs | Sink delay p99 µs |',
         '| --- | --- | --- | --- | --- | --- | --- |']
for count in [1, 8, 32, 64, 128, 256, 512]:
    groups = [[row for row in data if row['sessions'] == count] for data in [before, after]]
    assert all(len(group) == 3 for group in groups)
    for left, right in zip(*groups):
        assert left['audio_s'] == right['audio_s']
        assert left['packets'] == right['packets']
    cells = []
    for key in ['child_cpu_s', 'peak_sampled_rss_kib', 'peak_sampled_threads', 'startup_p99_us', 'sink_delay_p95_us', 'sink_delay_p99_us']:
        cells.append(' → '.join(f'{statistics.median(row[key] for row in group):.3f}' for group in groups))
    lines.append(f'| {count} | ' + ' | '.join(cells) + ' |')
lines += ['', '## Longer filtered playback with command churn', '',
          'One run per count, generated 15-second mono WAV, native tremolo filtering, complexity 9 / 128 kbps / VBR. Each session pauses immediately after play, then seeks to 100 ms while paused and resumes. 14.9 seconds per session are delivered. Startup includes the intentionally paused setup period, so it does not isolate decoder startup. Command percentiles combine pause, seek, and resume calls. The benchmark adds one resource sampler thread. Footprint is the maximum of one-second native samples, not USS or peak RSS.', '',
          '| Sessions | Wall s | CPU s | Mean process CPU % | Sampled RSS KiB | Sampled footprint B | Threads | Startup p99 µs | Sink p95 / p99 µs | Command p95 / p99 µs | Max collection ms |',
          '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |']
for path in ['benchmarks/phase2-sustained.jsonl', 'benchmarks/phase2-sustained-high.jsonl']:
    for row in map(json.loads, Path(path).read_text().splitlines()):
        samples = row['resource_samples']
        footprint = max(sample['memory']['footprintBytes'] or 0 for sample in samples)
        utilization = [sample['cpu']['processPercent'] for sample in samples if sample['cpu']['processPercent'] is not None]
        assert all(sample['memory']['privateResidentBytes'] is None for sample in samples)
        lines.append(f'| {row["sessions"]} | {row["wall_s"]:.3f} | {row["child_cpu_s"]:.3f} | {statistics.mean(utilization):.2f} | {row["peak_sampled_rss_kib"]} | {footprint} | {row["peak_sampled_threads"]} | {row["startup_p99_us"]} | {row["sink_delay_p95_us"]} / {row["sink_delay_p99_us"]} | {row["command_p95_us"]} / {row["command_p99_us"]} | {max(sample["collectionDurationMs"] for sample in samples):.6f} |')
Path('benchmarks/phase2-results.md').write_text('\n'.join(lines) + '\n')
