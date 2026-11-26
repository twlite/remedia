"""Observe concurrent stalled probes; excluded from scheduler performance comparisons.
Caller driver threads are counted separately from engine startup and CPU pool threads.
"""
import argparse
import ctypes
import hashlib
import json
import struct
import subprocess
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--original', required=True)
parser.add_argument('--scheduled', required=True)
parser.add_argument('--fixture', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--counts', default='128,512')
args = parser.parse_args()
libproc = ctypes.CDLL('/usr/lib/libproc.dylib')
libproc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
libproc.proc_pidinfo.restype = ctypes.c_int


def roles(pid):
    ids = ctypes.create_string_buffer(8*2048)
    size = libproc.proc_pidinfo(pid, 6, 0, ids, len(ids))
    result = dict(engineStartup=0, cpuWorkers=0, callerDrivers=0, other=0)
    for offset in range(0, max(0,size)-7, 8):
        tid = struct.unpack_from('Q',ids.raw,offset)[0]
        info = ctypes.create_string_buffer(112)
        if libproc.proc_pidinfo(pid,5,tid,info,len(info)) != 112:
            continue
        name = info.raw[48:112].split(b'\0',1)[0].decode(errors='replace')
        if name.startswith('remedia-playback-'):
            key = 'engineStartup'
        elif name.startswith('remedia-cpu-'):
            key = 'cpuWorkers'
        elif name.startswith('hybrid-start-driver-'):
            key = 'callerDrivers'
        else:
            key = 'other'
        result[key] += 1
    return result


manifest = dict(binaries={name:dict(path=path,sha256=hashlib.sha256(Path(path).read_bytes()).hexdigest()) for name,path in [('original',args.original),('scheduled',args.scheduled)]}, arguments=vars(args))
Path(args.output+'.manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
with Path(args.output).open('w') as output:
    for repeat in range(3):
        for count in map(int,args.counts.split(',')):
            for workers in [0,4]:
                binary = args.original if workers == 0 else args.scheduled
                child = subprocess.Popen([binary,args.fixture,str(count),str(workers)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
                marker = child.stdout.readline().strip()
                observed = None
                if marker == 'STARTUP_PROBES_WAITING':
                    deadline = time.monotonic()+15
                    while time.monotonic() < deadline:
                        observed = roles(child.pid)
                        if observed['engineStartup'] == count and observed['callerDrivers'] == count:
                            break
                        time.sleep(0.002)
                stdout,stderr = child.communicate(input='\n',timeout=180)
                if child.returncode or marker != 'STARTUP_PROBES_WAITING':
                    raise RuntimeError(stderr)
                if observed is None or observed['engineStartup'] != count or observed['callerDrivers'] != count:
                    raise RuntimeError(f'could not verify complete startup cohort: {observed}')
                row=json.loads(stdout)
                row.update(repetition=repeat,observedBeforeInputRelease=observed)
                if workers:
                    assert row['diagnostics'] == {'admitted':0,'unsupported':count,'inputFallbacks':count}
                output.write(json.dumps(row)+'\n')
                output.flush()
