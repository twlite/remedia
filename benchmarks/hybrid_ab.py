"""Matched original/refactored/scheduled release comparison, rotated within each case.
Sampling is in the parent process; CPU/context-switch deltas refer to the child alone.
Thread/RSS peaks are sampled at 2ms and can miss shorter transients.
"""
import argparse
import ctypes
import hashlib
import json
import resource
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--original', required=True)
parser.add_argument('--dedicated', required=True)
parser.add_argument('--scheduled', required=True)
parser.add_argument('--fixtures', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--counts', default='1,8,32,64,128,256,512')
parser.add_argument('--workers', default='1,2,4')
parser.add_argument('--budgets', default='1,8,32')
parser.add_argument('--formats', default='opus')
parser.add_argument('--repeats', type=int, default=3)
parser.add_argument('--source', choices=['bytes','file'], default='bytes')
parser.add_argument('--startup-stagger-ms', type=int, default=0)
parser.add_argument('--controls', type=int, default=0)
parser.add_argument('--mixed', action='store_true')
parser.add_argument('--pacing', choices=['unpaced','realtime'], default='unpaced')
args = parser.parse_args()
libproc = ctypes.CDLL('/usr/lib/libproc.dylib') if sys.platform == 'darwin' else None


def sample(pid):
    if libproc:
        info = ctypes.create_string_buffer(96)
        if libproc.proc_pidinfo(pid, 4, 0, info, 96) == 96:
            return struct.unpack_from('Q', info.raw, 8)[0], struct.unpack_from('i', info.raw, 84)[0]
    else:
        try:
            values = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
            return int(values['VmRSS'].split()[0]) * 1024, int(values['Threads'])
        except (FileNotFoundError, KeyError, ProcessLookupError):
            pass
    return None


def run(command):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    stop = threading.Event()
    samples = []

    def monitor():
        while not stop.is_set():
            value = sample(process.pid)
            if value:
                samples.append((time.time() * 1000, *value))
            stop.wait(0.002)

    thread = threading.Thread(target=monitor)
    thread.start()
    try:
        stdout, stderr = process.communicate(timeout=180)
    except BaseException:
        process.kill()
        process.communicate()
        raise
    finally:
        stop.set()
        thread.join()
    if process.returncode:
        raise RuntimeError(f'{command}: {stderr}')
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    row = json.loads(stdout)
    startup_end = row['samplingEpochUnixMs'] + row['startupSeconds'] * 1000
    startup = [s for s in samples if s[0] <= startup_end]
    row.update(sampledPeakThreads=max((s[2] for s in samples), default=None),
               sampledStartupPeakThreads=max((s[2] for s in startup), default=None),
               sampledPeakRssBytes=max((s[1] for s in samples), default=None),
               resourceSamples=len(samples), samplingIntervalMs=2,
               childCpuSeconds=after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
               voluntaryContextSwitches=after.ru_nvcsw - before.ru_nvcsw,
               involuntaryContextSwitches=after.ru_nivcsw - before.ru_nivcsw)
    return row


variants = [('original', args.original, 0, 1), ('dedicated', args.dedicated, 0, 1)]
variants += [('scheduled', args.scheduled, w, b) for w in map(int, args.workers.split(',')) for b in map(int, args.budgets.split(','))]
manifest = dict(binaries={name: dict(path=str(Path(path).resolve()), sha256=hashlib.sha256(Path(path).read_bytes()).hexdigest()) for name, path in [('original', args.original), ('dedicated', args.dedicated), ('scheduled', args.scheduled)]}, arguments=vars(args), fixtures={p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in args.fixtures.split(',')})
if Path(args.output).exists() or Path(args.output + '.manifest.json').exists():
    raise FileExistsError('choose a fresh output path to preserve previous results')
Path(args.output + '.manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
with Path(args.output).open('w') as output:
    for repeat in range(args.repeats):
        for fixture in (['|'.join(args.fixtures.split(','))] if args.mixed else args.fixtures.split(',')):
            for mode in args.formats.split(','):
                for count in map(int, args.counts.split(',')):
                    reference = None
                    # Rotation avoids always measuring the same implementation first/last.
                    offset = repeat % len(variants)
                    ordered = variants[offset:] + variants[:offset]
                    if repeat % 2:
                        ordered.reverse()
                    for backend, binary, workers, budget in ordered:
                        row = run([binary, fixture, str(count), str(workers), mode, str(budget), args.source, args.pacing, str(args.startup_stagger_ms), str(args.controls)])
                        if args.pacing == 'realtime' and mode != 'null' and row['processingSeconds'] < max(row['samples'])/48000-0.05:
                            raise RuntimeError('realtime output completed faster than its media clock')
                        delivered = (row['samples'], row['checksums'], row['chunks'])
                        if reference is None:
                            reference = delivered
                        if reference != delivered:
                            raise RuntimeError('delivered audio differs across implementations')
                        if backend == 'scheduled':
                            d = row['diagnostics']
                            if not row['startGate'] or row['activeScheduled'] != count or d['admitted'] != count or sum(d[k] for k in ['controlFallbacks', 'unsupportedFallbacks', 'capacityFallbacks']) or row['backends'] != {'scheduled': count}:
                                raise RuntimeError(f'fallback is not scheduler performance evidence: {row["backends"]}, {d}')
                        row.update(implementation=backend, fixture=fixture, repetition=repeat, deliveredAudioMatched=True)
                        output.write(json.dumps(row) + '\n')
                        output.flush()
