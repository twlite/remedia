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
            if d.get('pendingDeadlines', 0) != 0 or d.get('deadlinePeak', 0) > row['sessions']:
                raise ValueError('deadline storage was not reclaimed or exceeded admitted sessions')
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
    def percentile(values, fraction):
        values = sorted(v for v in values if v is not None)
        return values[int((len(values)-1)*fraction)] if values else None
    summary = dict(pacing=rows[0].get('pacing', 'Unpaced'), fixture=fixture, format=mode, sessions=count, implementation=implementation, workers=workers, budget=budget, repetitions=len(rows),
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
                   firstChunkP99Ms=med(lambda r:percentile(r.get('firstChunkLatencyMs', []), 0.99)),
                   deliveryP99MsFromFirstChunk=med(lambda r:r.get('deliveryP99MsFromFirstChunk')),
                   deliveryMaxMsFromFirstChunk=med(lambda r:r.get('deliveryMaxMsFromFirstChunk')),
                   worstInterChunkGapExcessMs=med(lambda r:max(r.get('worstGapExcessPerSessionMs', []), default=None)),
                   lastChunkMaxMs=med(lambda r:max((v for v in r.get('lastChunkMs', []) if v is not None), default=None)),
                   drainSeconds=med(lambda r:r.get('drainSeconds')), cleanupAfterEofSeconds=med(lambda r:r.get('cleanupAfterEofSeconds')),
                   deadlineSuspensions=med(lambda r:(r['diagnostics'] or {}).get('deadlineSuspensions')),
                   deadlineWakes=med(lambda r:(r['diagnostics'] or {}).get('deadlineWakes')),
                   deadlinePeak=med(lambda r:(r['diagnostics'] or {}).get('deadlinePeak')),
                   fallbackCount=med(lambda r:sum((r['diagnostics'] or {}).get(k,0) for k in ['controlFallbacks','unsupportedFallbacks','capacityFallbacks'])))
    baseline = groups[(fixture, mode, count, 'dedicated', 0, 1)]
    baseline_cpu = statistics.median(r['childCpuSeconds']*1000/r['audioSeconds'] for r in baseline)
    summary['cpuChangeVsRefactoredPercent'] = 100 * (summary['cpuMsPerAudioSecond']/baseline_cpu-1)
    summaries.append(summary)
Path(args.output).write_text(json.dumps(summaries, indent=2) + '\n')
