"""Interleaved fresh-process scheduler comparison with delivered-work validation.
Run builds separately first. OS peak RSS and context switches are kernel child counters;
thread and footprint fields are in-process snapshots and may miss short-lived peaks.
"""
import argparse
import json
import resource
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--fixtures', required=True, help='comma-separated independent encoded inputs')
parser.add_argument('--output', required=True)
parser.add_argument('--counts', default='1,8,32,64,128,256,512')
parser.add_argument('--workers', default='0,1,2,4')
parser.add_argument('--formats', default='null,s16le,f32le,opus')
parser.add_argument('--repeats', type=int, default=3)
args = parser.parse_args()
with Path(args.output).open('w') as output:
    for repeat in range(args.repeats):
        for fixture in args.fixtures.split(','):
            for mode in args.formats.split(','):
                for count in map(int, args.counts.split(',')):
                    reference = None
                    workers = list(map(int, args.workers.split(',')))
                    if repeat % 2:
                        workers.reverse()
                    for worker_count in workers:
                        before = resource.getrusage(resource.RUSAGE_CHILDREN)
                        child = subprocess.run([args.binary, fixture, str(count), str(worker_count), mode], capture_output=True, text=True, timeout=180, check=True)
                        after = resource.getrusage(resource.RUSAGE_CHILDREN)
                        row = json.loads(child.stdout)
                        delivered = (row['samples'], row['checksums'], row['chunks'])
                        if reference is None:
                            reference = delivered
                        if reference != delivered:
                            raise RuntimeError('backend delivered different audio')
                        row.update(repetition=repeat, fixture=fixture,
                                   childCpuSeconds=after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
                                   voluntaryContextSwitches=after.ru_nvcsw - before.ru_nvcsw,
                                   involuntaryContextSwitches=after.ru_nivcsw - before.ru_nivcsw)
                        output.write(json.dumps(row) + '\n')
                        output.flush()
