"""Separate Darwin diagnostic: sample named OS threads during startup.
This scanner performs many syscalls and is excluded from CPU/latency acceptance rows.
"""
import argparse
import ctypes
import json
import struct
import subprocess
import threading
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--original', required=True)
parser.add_argument('--scheduled', required=True)
parser.add_argument('--fixture', required=True)
parser.add_argument('--output', required=True)
args = parser.parse_args()
libproc = ctypes.CDLL('/usr/lib/libproc.dylib')
libproc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
libproc.proc_pidinfo.restype = ctypes.c_int


def roles(pid):
    ids = ctypes.create_string_buffer(8 * 2048)
    size = libproc.proc_pidinfo(pid, 6, 0, ids, len(ids))
    counts = dict(cpu=0, playback_or_startup=0, other=0)
    for offset in range(0, max(0, size) - 7, 8):
        tid = struct.unpack_from('Q', ids.raw, offset)[0]
        info = ctypes.create_string_buffer(112)
        if libproc.proc_pidinfo(pid, 5, tid, info, len(info)) != 112:
            continue
        name = info.raw[48:112].split(b'\0', 1)[0].decode(errors='replace')
        key = 'cpu' if name.startswith('remedia-cpu-') else 'playback_or_startup' if name.startswith('remedia-playback-') else 'other'
        counts[key] += 1
    return counts


with Path(args.output).open('w') as output:
    for repeat in range(3):
        for workers in [0, 1, 4, 10]:
            binary = args.original if workers == 0 else args.scheduled
            process = subprocess.Popen([binary, args.fixture, '512', str(workers), 'opus', '8', 'bytes', 'realtime'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            stop = threading.Event()
            samples = []

            def monitor():
                while not stop.is_set():
                    samples.append((time.time() * 1000, roles(process.pid)))
                    stop.wait(0.002)

            thread = threading.Thread(target=monitor)
            thread.start()
            try:
                stdout, stderr = process.communicate(timeout=180)
            finally:
                stop.set()
                thread.join()
            if process.returncode:
                raise RuntimeError(stderr)
            row = json.loads(stdout)
            end = row['samplingEpochUnixMs'] + row['startupSeconds']*1000
            startup = [s for s in samples if s[0] <= end]
            peaks = {k:max((s[1][k] for s in startup), default=0) for k in ['cpu','playback_or_startup','other']}
            if workers:
                assert row['diagnostics']['admitted'] == 512 and row['diagnostics']['unsupportedFallbacks'] == 0
            result = dict(profilingOnly=True, sessions=512, workers=workers, repetition=repeat,
                          startupRolePeaks=peaks, transientStartupPeak=peaks['playback_or_startup'] if workers else None,
                          startupRoleSamples=startup, minimumSamplingIntervalMs=2,
                          actualIntervalIncludesScannerTime=True, activeThreads=row['active']['cpu']['threadCount'],
                          scheduledAdmissions=(row['diagnostics'] or {}).get('admitted', 0))
            output.write(json.dumps(result)+'\n')
            output.flush()
