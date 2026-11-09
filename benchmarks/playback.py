"""Run the release example, sampling process RSS/threads on macOS/Linux."""
import argparse
import ctypes
import json
import resource
import struct
import subprocess
import sys
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('--binary', default='target/release/examples/concurrent_playback_benchmark')
parser.add_argument('--output', required=True)
parser.add_argument('--counts', default='1,8,32,64,128')
parser.add_argument('--repeats', type=int, default=3)
parser.add_argument('--workloads', default='passthrough,volume,filter')
parser.add_argument('--fixtures', default='generated-opus-passthrough.ogg,generated-tone.mp3')
parser.add_argument('--pacing', default='unpaced,paced')
parser.add_argument('--controls', action='store_true')
parser.add_argument('--complexity', type=int, default=9)
args = parser.parse_args()

if args.repeats < 1:
    parser.error('repeats must be positive')

with open(args.output, 'w') as output:
    for repeat in range(args.repeats):
        for fixture in args.fixtures.split(','):
            for workload in args.workloads.split(','):
                for pacing in args.pacing.split(','):
                    for count in map(int, args.counts.split(',')):
                        before = resource.getrusage(resource.RUSAGE_CHILDREN)
                        process = subprocess.Popen([
                            args.binary,
                            str(Path('crates/remedia-core/tests/fixtures') / fixture),
                            str(count), workload, pacing, str(args.complexity),
                            *(['controls'] if args.controls else []),
                        ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                        rss = threads = 0
                        deadline = time.monotonic() + 60
                        while process.poll() is None:
                            if time.monotonic() > deadline:
                                process.kill()
                                process.communicate()
                                raise TimeoutError('benchmark exceeded 60 seconds')
                            if sys.platform == 'darwin':
                                libproc = ctypes.CDLL('/usr/lib/libproc.dylib')
                                info = ctypes.create_string_buffer(96)
                                if libproc.proc_pidinfo(process.pid, 4, 0, info, 96) == 96:
                                    rss = max(rss, struct.unpack_from('Q', info.raw, 8)[0] // 1024)
                                    threads = max(threads, struct.unpack_from('i', info.raw, 84)[0])
                            else:
                                try:
                                    with open(f'/proc/{process.pid}/status') as status:
                                        for line in status:
                                            if line.startswith('VmRSS:'):
                                                rss = max(rss, int(line.split()[1]))
                                            if line.startswith('Threads:'):
                                                threads = max(threads, int(line.split()[1]))
                                except FileNotFoundError:
                                    pass
                            time.sleep(0.02)
                        stdout, stderr = process.communicate()
                        if process.returncode:
                            raise RuntimeError(stderr)
                        after = resource.getrusage(resource.RUSAGE_CHILDREN)
                        row = dict(complexity=args.complexity, repeat=repeat, fixture=fixture, workload=workload, pacing=pacing, sessions=count, peak_sampled_rss_kib=rss, peak_sampled_threads=threads,
                                   child_cpu_s=after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime, **json.loads(stdout))
                        output.write(json.dumps(row) + '\n')
                        output.flush()
                        print(f'{fixture} {workload} {pacing} {count}: {row["wall_s"]:.3f}s', flush=True)
