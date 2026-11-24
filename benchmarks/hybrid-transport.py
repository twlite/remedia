"""Release A/B of the real Discord sender/router queue boundary and controlled async consumer.
No network/DAVE cost is represented. Normal RTP/DAVE loopback tests verify packet correctness.
Run the built test executable directly; cargo/build work must finish before measurements.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import resource
import struct
import subprocess
import sys
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--fixtures', required=True)
parser.add_argument('--counts', default='1,8,32,64,128,256,512')
parser.add_argument('--workers', default='2,4,10')
parser.add_argument('--budgets', default='1,8,32')
parser.add_argument('--cases', default='unpaced,paced,stagger,stalls')
parser.add_argument('--repeats', type=int, default=3)
parser.add_argument('--output', required=True)
args = parser.parse_args()
if Path(args.output).exists() or Path(args.output + '.manifest.json').exists():
    raise FileExistsError('choose a fresh output path')
manifest = dict(arguments=vars(args), binarySha256=hashlib.sha256(Path(args.binary).read_bytes()).hexdigest(), fixtures={path: hashlib.sha256(Path(path).read_bytes()).hexdigest() for path in args.fixtures.split(',')})
Path(args.output + '.manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
libproc = ctypes.CDLL('/usr/lib/libproc.dylib') if sys.platform == 'darwin' else None


def sample(pid):
    if libproc:
        info = ctypes.create_string_buffer(96)
        if libproc.proc_pidinfo(pid, 4, 0, info, 96) == 96:
            return struct.unpack_from('Q', info.raw, 8)[0], struct.unpack_from('i', info.raw, 84)[0]
    else:
        try:
            status = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
            return int(status['VmRSS'].split()[0]) * 1024, int(status['Threads'])
        except (FileNotFoundError, KeyError, ProcessLookupError):
            pass
    return None


def run(fixture, count, workers, budget, case):
    env = dict(os.environ, HYBRID_FIXTURE=fixture, HYBRID_SESSIONS=str(count), HYBRID_WORKERS=str(workers), HYBRID_BUDGET=str(budget), HYBRID_PACED=str(int(case != 'unpaced')), HYBRID_STAGGER=str(int(case == 'stagger')), HYBRID_STALLS=str(int(case == 'stalls')))
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    process = subprocess.Popen([args.binary, 'scheduled_transport_queue_benchmark', '--ignored', '--nocapture', '--test-threads=1'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    samples = []
    stop = threading.Event()

    def monitor():
        while not stop.is_set():
            value = sample(process.pid)
            if value:
                samples.append((time.time() * 1000, *value))
            stop.wait(0.002)

    observer = threading.Thread(target=monitor)
    observer.start()
    try:
        stdout, stderr = process.communicate(timeout=180)
    except BaseException:
        process.kill()
        process.communicate()
        raise
    finally:
        stop.set()
        observer.join()
    if process.returncode:
        raise RuntimeError(stdout + stderr)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    row = json.loads(stdout.split('HYBRID_RESULT=', 1)[1].splitlines()[0])
    startup_end = row['samplingEpochUnixMs'] + row['startupSeconds'] * 1000
    row.update(case=case, sampledPeakThreads=max((s[2] for s in samples), default=None), sampledStartupPeakThreads=max((s[2] for s in samples if s[0] <= startup_end), default=None), sampledPeakRssBytes=max((s[1] for s in samples), default=None), samplingIntervalMs=2, resourceSamples=len(samples), childCpuSeconds=after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime, voluntaryContextSwitches=after.ru_nvcsw-before.ru_nvcsw, involuntaryContextSwitches=after.ru_nivcsw-before.ru_nivcsw)
    if workers:
        diagnostics = row['diagnostics']
        if diagnostics['admitted'] != count or sum(diagnostics[k] for k in ['controlFallbacks', 'unsupportedFallbacks', 'capacityFallbacks']):
            raise ValueError('fallback is not scheduler performance evidence')
    return row


variants = [(0, 1)] + [(w, b) for w in map(int, args.workers.split(',')) for b in map(int, args.budgets.split(','))]
with Path(args.output).open('w') as output:
    for fixture in args.fixtures.split(','):
        for case in args.cases.split(','):
            for count in map(int, args.counts.split(',')):
                reference = None
                for repeat in range(args.repeats):
                    ordered = variants[repeat % len(variants):] + variants[:repeat % len(variants)]
                    if repeat % 2:
                        ordered.reverse()
                    for workers, budget in ordered:
                        row = run(fixture, count, workers, budget, case)
                        delivered = row['samples'], row['checksums'], row['packets']
                        if reference is None:
                            reference = delivered
                        if reference != delivered:
                            raise ValueError('delivered audio differs')
                        row.update(repetition=repeat, deliveredAudioMatched=True)
                        output.write(json.dumps(row) + '\n')
                        output.flush()
