"""Alternating A/B of unpaced (work-conserving) CPU across benchmark binaries.

Paced runs inflate and scatter CPU time (timer wake-ups, clock scaling at low load), so builds that
differ by a few percent cannot be told apart. Unpaced runs measure the work itself; alternating the
binaries every repetition cancels drift. CPU is the child's exact `wait4` user+system time.

Usage: ab_unpaced.py --fixtures DIR --reps N NAME=PATH [NAME=PATH ...]
"""
import argparse
import json
import os
import statistics
import subprocess

WORKLOADS = {
    # name: (fixture, sessions, workload)
    'mp3 transcode x4': ('music60.mp3', 4, 'passthrough'),
    'aac transcode x4': ('music60.m4a', 4, 'passthrough'),
    'mp3 + tremolo x4': ('music60.mp3', 4, 'filter'),
    'real Opus x4 (pre-skip 312)': ('music60.opus', 4, 'passthrough'),
    'pre-skip-0 Opus x128 packets': ('music60-pt.opus', 128, 'passthrough'),
}

parser = argparse.ArgumentParser()
parser.add_argument('--fixtures', required=True)
parser.add_argument('--reps', type=int, default=10)
parser.add_argument('binaries', nargs='+')
args = parser.parse_args()
binaries = [item.split('=', 1) for item in args.binaries]


def run(path, fixture, sessions, workload):
    command = [path, os.path.join(args.fixtures, fixture), str(sessions), workload, 'unpaced', '9']
    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read_fd)
        os.dup2(write_fd, 1)
        os.execv(command[0], command)
    os.close(write_fd)
    with os.fdopen(read_fd, 'rb') as stream:
        output = stream.read()
    _, status, usage = os.wait4(pid, 0)
    assert status == 0, command
    return usage.ru_utime + usage.ru_stime, json.loads(output)['packets']


print('| Workload | ' + ' | '.join(name for name, _ in binaries) + ' |')
print('| --- |' + ' --- |' * len(binaries))
for label, (fixture, sessions, workload) in WORKLOADS.items():
    samples = {name: [] for name, _ in binaries}
    packets = {}
    for rep in range(args.reps):
        order = binaries[rep % len(binaries):] + binaries[:rep % len(binaries)]
        for name, path in order:
            cpu, count = run(path, fixture, sessions, workload)
            samples[name].append(cpu)
            packets[name] = count
    base = statistics.median(samples[binaries[0][0]])
    cells = []
    for name, _ in binaries:
        median = statistics.median(samples[name])
        spread = (max(samples[name]) - min(samples[name])) / median * 100
        cells.append(f'{median:.3f} s ({(median - base) / base * 100:+.1f}%, ±{spread / 2:.0f}%)')
    print(f'| {label} | ' + ' | '.join(cells) + ' |')
