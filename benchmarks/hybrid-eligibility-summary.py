"""Summarize only completed final eligibility suites; raw per-session rows stay intact."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median

parser = argparse.ArgumentParser()
parser.add_argument('directory', type=Path)
parser.add_argument('--output', required=True, type=Path)
parser.add_argument('--qualification-output', type=Path)
args = parser.parse_args()

def percentile(values, fraction):
    values = sorted(v for v in values if v is not None)
    return values[min(len(values) - 1, int((len(values) - 1) * fraction))] if values else None

result = []
qualified_rows = 0
expected_samples = {'music3.mp3':144000, 'music3.opus':144000, 'music3.aac':145408, 'music15.opus':720000}
for path in sorted(args.directory.glob('final-*.jsonl')):
    groups = defaultdict(list)
    delivery_references = {}
    for line in path.read_text().splitlines():
        row = json.loads(line)
        assert row['deliveredAudioMatched']
        qualified_rows += 1
        identity = (row['fixture'], row.get('format','discord'), row.get('case',row.get('pacing')),row['sessions'])
        delivered = (row['samples'],row['checksums'],row.get('chunks',row.get('packets')))
        assert delivery_references.setdefault(identity,delivered) == delivered, (path, identity, 'cross-repetition delivery mismatch')
        sources = row['fixture'].split('|')
        for index, samples in enumerate(row['samples']):
            name = Path(sources[index % len(sources)]).name
            if name in expected_samples:
                expected = expected_samples[name]
                rounds = row.get('controlRounds',0)
                if rounds:
                    expected -= (100+(rounds-1)%4*10)*48
                assert samples == expected, (path,name,samples,expected)
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
        private = [r['active']['memory']['privateResidentBytes'] for r in rows if r['active']['memory']['privateResidentBytes'] is not None]
        item['activePrivateMiB'] = median(private)/1048576 if private else None
        private_deltas = [(r['active']['memory']['privateResidentBytes']-r['before']['memory']['privateResidentBytes'])/1048576 for r in rows if r['active']['memory']['privateResidentBytes'] is not None and r['before']['memory']['privateResidentBytes'] is not None]
        item['incrementalPrivateMiB'] = median(private_deltas) if private_deltas else None
        item['scheduler'] = {k:median(r[k] for r in d) for k in ['queuePeak','queueContentions','queueWaitNs','turns','actions','workerBusyNs']} if d else None
        if d:
            item['scheduler']['busyFractionOfWorkerWallTime'] = median(r['diagnostics']['workerBusyNs']/1e9/r['wallSeconds']/workers for r in rows)
        result.append(item)
args.output.write_text(json.dumps(result,indent=2)+'\n')

if args.qualification_output:
    args.qualification_output.write_text(json.dumps(dict(qualifiedRows=qualified_rows, exactSamplesVerified=True, crossRepetitionChecksumsMatched=True, expectedSampleFrames=expected_samples, source="Native synthetic fixtures, normalized 48 kHz stereo; control rows subtract final seek target"),indent=2)+'\n')
