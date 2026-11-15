"""Run the resource benchmark matrix against one benchmark executable.

Every case is a fresh process so peak RSS and CPU belong to that case alone. CPU and peak
RSS come from `wait4` (exact kernel accounting for the child), not from sampling. Heap counters
(`--alloc`) change timing slightly, so they are collected in separate rows from CPU/RSS rows.
"""
import argparse
import json
import os
import sys
from pathlib import Path

CASES = {
    # name: (fixture, workload, seconds)
    'opus-passthrough': ('music60-pt.opus', 'passthrough', 20),
    'opus-crossfade-idle': ('music60-pt.opus', 'crossfade', 20),
    'opus-volume': ('music60-pt.opus', 'volume', 20),
    'opus-seek': ('music60-pt.opus', 'seek', 30),
    'opus-transition': ('music15-pt.opus', 'transition', 36),
    # Real FFmpeg/libopus files carry a 312-sample pre-skip; the -pt files above were rewritten to 0.
    'opus-real-passthrough': ('music60.opus', 'passthrough', 20),
    'opus-real-seek': ('music60.opus', 'seek', 30),
    'opus-real-transition': ('music15.opus', 'transition', 36),
    'mp3-transcode': ('music60.mp3', 'passthrough', 20),
    'aac-transcode': ('music60.m4a', 'passthrough', 20),
    'mp3-filter': ('music60.mp3', 'filter', 20),
    'mp3-stream': ('music60.mp3', 'stream', 20),
}

parser = argparse.ArgumentParser()
parser.add_argument('--binary', default='target/release/examples/concurrent_playback_benchmark')
parser.add_argument('--fixtures', required=True, help='directory holding the generated fixtures')
parser.add_argument('--output', required=True)
parser.add_argument('--cases', default=','.join(CASES))
parser.add_argument('--counts', default='1,32,128,512')
parser.add_argument('--repeats', type=int, default=3)
parser.add_argument('--alloc', action='store_true', help='enable heap counters (separate rows)')
parser.add_argument('--label', default='')
args = parser.parse_args()


def run_with_rusage(case, count):
    fixture, workload, seconds = CASES[case]
    command = [args.binary, str(Path(args.fixtures) / fixture), str(count), workload, 'paced',
               '9', f'seconds={seconds}'] + (['alloc'] if args.alloc else [])
    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read_fd)
        os.dup2(write_fd, 1)
        os.execv(command[0], command)
    os.close(write_fd)
    chunks = []
    with os.fdopen(read_fd, 'rb') as stream:
        chunks.append(stream.read())
    _, status, usage = os.wait4(pid, 0)
    if status != 0:
        raise RuntimeError(f'{command} exited with status {status}')
    peak = usage.ru_maxrss if sys.platform == 'darwin' else usage.ru_maxrss * 1024
    return json.loads(b''.join(chunks)), usage.ru_utime + usage.ru_stime, peak


with open(args.output, 'a') as output:
    for repeat in range(args.repeats):
        for case in args.cases.split(','):
            for count in map(int, args.counts.split(',')):
                result, cpu, peak_rss = run_with_rusage(case, count)
                samples = result.pop('resource_samples')
                # Steady state: ignore the first three one-second samples (session startup).
                steady = samples[3:] if len(samples) > 5 else samples
                cpu_ms = lambda s: s['cpu']['userTimeMs'] + s['cpu']['systemTimeMs']
                steady_cpu_s = (cpu_ms(steady[-1]) - cpu_ms(steady[0])) / 1000 if len(steady) > 1 else None
                steady_wall = (steady[-1]['collectedAtUnixMs'] - steady[0]['collectedAtUnixMs']) / 1000
                row = dict(
                    label=args.label, case=case, sessions=count, repeat=repeat, alloc=args.alloc,
                    cpu_s=cpu, steady_cpu_s=steady_cpu_s, steady_wall_s=steady_wall,
                    peak_rss_bytes=peak_rss,
                    max_footprint_bytes=max((s['memory']['footprintBytes'] or 0) for s in samples),
                    max_threads=max(s['cpu']['threadCount'] or 0 for s in samples), **result)
                output.write(json.dumps(row) + '\n')
                output.flush()
                print(f"{case} x{count} r{repeat}: cpu={cpu:.2f}s steady={steady_cpu_s} "
                      f"rss={peak_rss/1048576:.1f}MiB threads={row['max_threads']} "
                      f"pt={row['passthrough_fraction']}", flush=True)
