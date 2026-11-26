"""Emit Markdown medians; do not infer acceptance from unmeasured workloads."""
import argparse
import collections
import json
import statistics

parser = argparse.ArgumentParser()
parser.add_argument('path')
args = parser.parse_args()
groups = collections.defaultdict(list)
for line in open(args.path):
    row = json.loads(line)
    groups[(row['format'], row['sessions'], row['workers'])].append(row)
print('| Format | Sessions | Workers | Reps | CPU ms/audio s | Wall s | End RSS MiB | End footprint MiB | End threads |')
print('| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |')
for (mode, count, workers), rows in sorted(groups.items()):
    def median(get):
        values = [get(row) for row in rows if get(row) is not None]
        return statistics.median(values) if values else float('nan')
    print(f"| {mode} | {count} | {workers} | {len(rows)} | {median(lambda r:r['cpuMsPerAudioSecond']):.3f} | {median(lambda r:r['wallSeconds']):.3f} | {median(lambda r:r['after']['memory']['residentBytes']/1048576):.2f} | {median(lambda r:(r['after']['memory']['footprintBytes'] or 0)/1048576):.2f} | {median(lambda r:r['after']['cpu']['threadCount']):.0f} |")
