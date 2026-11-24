"""Summarize only completed final eligibility suites; raw per-session rows stay intact."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median

parser = argparse.ArgumentParser()
parser.add_argument('directory', type=Path)
parser.add_argument('--output', required=True, type=Path)
args = parser.parse_args()

def percentile(values, fraction):
    values = sorted(v for v in values if v is not None)
    return values[min(len(values) - 1, int((len(values) - 1) * fraction))] if values else None

result = []
for path in sorted(args.directory.glob('final-*.jsonl')):
    groups = defaultdict(list)
    for line in path.read_text().splitlines():
        row = json.loads(line)
        assert row['deliveredAudioMatched']
        fixture = '|'.join(Path(p).name for p in row['fixture'].split('|'))
        key = (row.get('implementation', 'scheduled' if row['workers'] else 'dedicated'), fixture, row.get('format', 'discord'), row.get('case', row.get('pacing')), row['sessions'], row['workers'], row['budget'])
        groups[key].append(row)
    for (implementation, fixture, fmt, pacing, count, workers, budget), rows in groups.items():
        assert len(rows) == 3, (path, count, workers, budget, len(rows))
        d = [r['diagnostics'] for r in rows if r['diagnostics']]
        if workers:
            assert len(d) == 3 and all(r['admitted'] == count and not sum(r[k] for k in ['controlFallbacks', 'unsupportedFallbacks', 'capacityFallbacks']) and not r.get('probeRetries', 0) for r in d)
        med = lambda name: median(r[name] for r in rows)
        arrays = lambda name: [v for r in rows for v in r.get(name, [])]
        nested = lambda phase, kind, field: median(r[phase][kind][field] for r in rows if r[phase][kind][field] is not None)
        item = dict(suite=path.stem, implementation=implementation, fixture=fixture, format=fmt, pacing=pacing, sessions=count, workers=workers, budget=budget, repetitions=3,
                    cpuMsPerAudioSecond=median(r['childCpuSeconds'] * 1000 / r['audioSeconds'] for r in rows), wallSeconds=med('wallSeconds'),
                    throughputAudioSecondsPerWallSecond=median(r['audioSeconds']/r['wallSeconds'] for r in rows),
                    activeThreads=nested('active','cpu','threadCount'), peakThreads=med('sampledPeakThreads'), startupPeakThreads=med('sampledStartupPeakThreads'),
                    peakRssMiB=med('sampledPeakRssBytes')/1048576,
                    incrementalActiveRssMiB=median((r['active']['memory']['residentBytes']-r['before']['memory']['residentBytes'])/1048576 for r in rows),
                    voluntarySwitches=med('voluntaryContextSwitches'), involuntarySwitches=med('involuntaryContextSwitches'),
                    startupP99Ms=percentile(arrays('startupLatenciesMs'), .99),
                    gapP99Ms=percentile(arrays('worstGapExcessPerSessionMs') or arrays('worstGapExcessMs'),.99),
                    latenessP99Ms=percentile(arrays('worstLatenessPerSessionMs') or arrays('worstLatenessMs'),.99),
                    admitted=count if workers else 0, fallbacks=0 if workers else None,
                    controlP99Ms={k:percentile([v for r in rows for v in r.get('controlLatenciesMs',{}).get(k,[])],.99) for k in rows[0].get('controlLatenciesMs',{})})
        physical = [r['active']['memory']['footprintBytes'] for r in rows if r['active']['memory']['footprintBytes'] is not None]
        item['activeFootprintMiB'] = median(physical)/1048576 if physical else None
        item['scheduler'] = {k:median(r[k] for r in d) for k in ['queuePeak','queueContentions','queueWaitNs','turns','actions','workerBusyNs']} if d else None
        if d:
            item['scheduler']['busyFractionOfWorkerWallTime'] = median(r['diagnostics']['workerBusyNs']/1e9/r['wallSeconds']/workers for r in rows)
        result.append(item)
args.output.write_text(json.dumps(result,indent=2)+'\n')
