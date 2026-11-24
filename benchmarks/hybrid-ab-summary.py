"""Summarize complete matched comparison rows without mixing fallback or pilot data."""
import argparse
import collections
import json
import statistics
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('paths', nargs='+')
parser.add_argument('--output', required=True)
parser.add_argument('--repeats', type=int, default=3)
args = parser.parse_args()
groups = collections.defaultdict(list)
for path in args.paths:
    for line in Path(path).read_text().splitlines():
        row = json.loads(line)
        if not row.get('startGate') or not row['deliveredAudioMatched']:
            raise ValueError('unqualified comparison row')
        if row['implementation'] == 'scheduled':
            d = row['diagnostics']
            if row['activeScheduled'] != row['sessions'] or d['admitted'] != row['sessions'] or sum(d[k] for k in ['controlFallbacks', 'unsupportedFallbacks', 'capacityFallbacks']):
                raise ValueError('fallback or incomplete scheduled admission')
        key = (row['fixture'], row['format'], row['sessions'], row['implementation'], row['workers'], row['budget'])
        groups[key].append(row)

summaries = []
for (fixture, mode, count, implementation, workers, budget), rows in sorted(groups.items()):
    if len(rows) != args.repeats:
        raise ValueError(f'incomplete group: {(fixture, mode, count, implementation, workers, budget)}')
    def med(get):
        values = [get(r) for r in rows if get(r) is not None]
        return statistics.median(values) if values else None
    summary = dict(fixture=fixture, format=mode, sessions=count, implementation=implementation, workers=workers, budget=budget, repetitions=len(rows),
                   cpuMsPerAudioSecond=med(lambda r:r['childCpuSeconds'] * 1000 / r['audioSeconds']),
                   cpuRange=[min(r['childCpuSeconds']*1000/r['audioSeconds'] for r in rows), max(r['childCpuSeconds']*1000/r['audioSeconds'] for r in rows)],
                   wallSeconds=med(lambda r:r['wallSeconds']), processingSeconds=med(lambda r:r['processingSeconds']), startupSeconds=med(lambda r:r['startupSeconds']),
                   activeScheduled=med(lambda r:r['activeScheduled']), sampledPeakThreads=med(lambda r:r['sampledPeakThreads']), sampledStartupPeakThreads=med(lambda r:r['sampledStartupPeakThreads']),
                   activeThreads=med(lambda r:r['active']['cpu']['threadCount']), peakRssMiB=med(lambda r:r['sampledPeakRssBytes']/1048576),
                   incrementalActiveRssMiB=med(lambda r:(r['active']['memory']['residentBytes']-r['before']['memory']['residentBytes'])/1048576),
                   activeFootprintMiB=med(lambda r:r['active']['memory']['footprintBytes']/1048576),
                   voluntaryContextSwitches=med(lambda r:r['voluntaryContextSwitches']), involuntaryContextSwitches=med(lambda r:r['involuntaryContextSwitches']),
                   turns=med(lambda r:(r['diagnostics'] or {}).get('turns')), actions=med(lambda r:(r['diagnostics'] or {}).get('actions')),
                   queueContentions=med(lambda r:(r['diagnostics'] or {}).get('queueContentions')), queueWaitNs=med(lambda r:(r['diagnostics'] or {}).get('queueWaitNs')),
                   fallbackCount=med(lambda r:sum((r['diagnostics'] or {}).get(k,0) for k in ['controlFallbacks','unsupportedFallbacks','capacityFallbacks'])))
    baseline = groups[(fixture, mode, count, 'dedicated', 0, 1)]
    baseline_cpu = statistics.median(r['childCpuSeconds']*1000/r['audioSeconds'] for r in baseline)
    summary['cpuChangeVsRefactoredPercent'] = 100 * (summary['cpuMsPerAudioSecond']/baseline_cpu-1)
    summaries.append(summary)
Path(args.output).write_text(json.dumps(summaries, indent=2) + '\n')
