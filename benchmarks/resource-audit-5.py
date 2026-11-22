"""Generate deterministic audio and run the Opus complexity resource audit."""

import argparse
import hashlib
import json
import math
import os
import struct
import subprocess
import sys
import wave
from pathlib import Path

SAMPLE_RATE = 48_000
OPUS_FRAME_SAMPLES = 960
COMPLEXITIES = (5, 6, 9)
DEFAULT_COUNTS = (1, 8, 32)


def generate_fixture(path, seconds):
    """Write deterministic 48 kHz stereo PCM with layered, changing tones and noise."""
    path.parent.mkdir(parents=True, exist_ok=True)
    state = 0xA341316C
    frames = bytearray()

    with wave.open(str(path), 'wb') as output:
        output.setnchannels(2)
        output.setsampwidth(2)
        output.setframerate(SAMPLE_RATE)

        for index in range(seconds * SAMPLE_RATE):
            time = index / SAMPLE_RATE
            state ^= (state << 13) & 0xFFFFFFFF
            state ^= state >> 17
            state ^= (state << 5) & 0xFFFFFFFF
            noise = ((state & 0xFFFF) / 32767.5) - 1.0

            left = (
                0.19 * math.sin(2 * math.pi * 220 * time) * (0.62 + 0.38 * math.sin(2 * math.pi * 0.47 * time))
                + 0.14 * math.sin(2 * math.pi * 330 * time + 0.35 * math.sin(2 * math.pi * 2.9 * time))
                + 0.09 * math.sin(2 * math.pi * (440 * time + 9 * (1 - math.cos(2 * math.pi * 0.31 * time))))
                + 0.025 * noise
            )
            right = (
                0.18 * math.sin(2 * math.pi * 277 * time) * (0.58 + 0.42 * math.sin(2 * math.pi * 0.39 * time))
                + 0.13 * math.sin(2 * math.pi * 392 * time + 0.28 * math.sin(2 * math.pi * 2.3 * time))
                + 0.08 * math.sin(2 * math.pi * (523 * time + 7 * (1 - math.cos(2 * math.pi * 0.27 * time))))
                + 0.025 * noise
            )
            frames.extend(struct.pack('<hh', round(left * 32767), round(right * 32767)))

            if len(frames) >= 1_048_576:
                output.writeframesraw(frames)
                frames.clear()

        if frames:
            output.writeframesraw(frames)


def run_child(binary, fixture, sessions, complexity, alloc):
    command = [str(binary), str(fixture), str(sessions), 'volume', 'unpaced', str(complexity)]
    if alloc:
        command.append('alloc')

    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read_fd)
        os.dup2(write_fd, 1)
        os.close(write_fd)
        try:
            os.execv(command[0], command)
        except Exception as error:
            os.write(2, f'could not execute benchmark: {error}\n'.encode())
            os._exit(127)

    os.close(write_fd)
    with os.fdopen(read_fd, 'rb') as stream:
        output = stream.read()
    _, status, usage = os.wait4(pid, 0)
    if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
        raise RuntimeError(f'{command} exited with status {status}')

    result = json.loads(output)
    samples = result.pop('resource_samples')
    peak_rss = usage.ru_maxrss if sys.platform == 'darwin' else usage.ru_maxrss * 1024
    result['cpu_s'] = usage.ru_utime + usage.ru_stime
    result['peak_rss_bytes'] = peak_rss
    result['max_footprint_bytes'] = max(
        (sample['memory']['footprintBytes'] or 0 for sample in samples), default=0
    )
    result['max_threads'] = max(
        (sample['cpu']['threadCount'] or 0 for sample in samples), default=0
    )
    return result


def write_fixture(args):
    generate_fixture(args.path.resolve(), args.seconds)
    digest = hashlib.sha256(args.path.read_bytes()).hexdigest()
    print(json.dumps({
        'fixture': str(args.path.resolve()),
        'seconds': args.seconds,
        'sample_rate': SAMPLE_RATE,
        'channels': 2,
        'sha256': digest,
    }))


def run_matrix(args):
    binary = args.binary.resolve()
    fixture = args.fixture.resolve()
    with wave.open(str(fixture), 'rb') as source:
        seconds = source.getnframes() / source.getframerate()
    fixture_hash = hashlib.sha256(fixture.read_bytes()).hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)

    with args.output.open('a', encoding='utf-8') as output:
        for repeat in range(args.repeats):
            complexities = COMPLEXITIES[repeat % 3:] + COMPLEXITIES[:repeat % 3]
            counts = args.counts[repeat % len(args.counts):] + args.counts[:repeat % len(args.counts)]
            for complexity in complexities:
                for sessions in counts:
                    result = run_child(binary, fixture, sessions, complexity, args.alloc)
                    audio_samples = round(result['audio_s'] * SAMPLE_RATE)
                    expected_for_case = source_frames(fixture) * sessions
                    expected_packets = expected_for_case // OPUS_FRAME_SAMPLES
                    if audio_samples != expected_for_case or result['packets'] != expected_packets:
                        raise RuntimeError(
                            f'complexity {complexity}, {sessions} sessions delivered '
                            f'{audio_samples} samples/{result["packets"]} packets; expected '
                            f'{expected_for_case}/{expected_packets}'
                        )
                    if result['passthrough_fraction'] not in (None, 0, 0.0):
                        raise RuntimeError('volume workload unexpectedly selected passthrough')

                    row = {
                        'label': args.label,
                        'profile': 'release',
                        'lto': 'fat',
                        'fixture_sha256': fixture_hash,
                        'fixture_seconds': seconds,
                        'complexity': complexity,
                        'sessions': sessions,
                        'repeat': repeat,
                        'alloc': args.alloc,
                        **result,
                        'audio_samples': audio_samples,
                        'expected_audio_samples': expected_for_case,
                        'expected_packets': expected_packets,
                    }
                    output.write(json.dumps(row, separators=(',', ':')) + '\n')
                    output.flush()
                    print(
                        f'{args.label} complexity={complexity} sessions={sessions} '
                        f'repeat={repeat}: cpu={row["cpu_s"]:.3f}s '
                        f'audio={row["audio_s"]:.1f}s packets={row["packets"]} '
                        f'rss={row["peak_rss_bytes"] / 1048576:.1f}MiB',
                        flush=True,
                    )


def run_quality(args):
    binary = args.binary.resolve()
    fixture = args.fixture.resolve()
    fixture_hash = hashlib.sha256(fixture.read_bytes()).hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)

    with args.output.open('a', encoding='utf-8') as output:
        for complexity in COMPLEXITIES:
            command = [str(binary), str(fixture), str(complexity)]
            result = subprocess.run(command, check=True, capture_output=True, text=True)
            measurements = json.loads(result.stdout)
            row = {
                'label': args.label,
                'profile': 'release',
                'fixture_sha256': fixture_hash,
                **measurements,
            }
            output.write(json.dumps(row, separators=(',', ':')) + '\n')
            output.flush()
            print(
                f'{args.label} complexity={complexity}: snr={row["snr_db"]:.3f}dB '
                f'p10={row["segment_snr_p10_db"]:.3f}dB packets={row["packet_count"]}',
                flush=True,
            )


def source_frames(path):
    with wave.open(str(path), 'rb') as source:
        return source.getnframes()


parser = argparse.ArgumentParser()
subparsers = parser.add_subparsers(required=True)

fixture_parser = subparsers.add_parser('fixture', help='generate deterministic music-like PCM')
fixture_parser.add_argument('path', type=Path)
fixture_parser.add_argument('--seconds', type=int, default=60)
fixture_parser.set_defaults(action=write_fixture)

run_parser = subparsers.add_parser('run', help='run repeated concurrent complexity measurements')
run_parser.add_argument('--binary', type=Path, required=True)
run_parser.add_argument('--fixture', type=Path, required=True)
run_parser.add_argument('--output', type=Path, required=True)
run_parser.add_argument('--label', choices=('before', 'after'), required=True)
run_parser.add_argument('--counts', type=lambda value: tuple(map(int, value.split(','))), default=DEFAULT_COUNTS)
run_parser.add_argument('--repeats', type=int, default=3)
run_parser.add_argument('--alloc', action='store_true', help='collect Rust allocator counters separately')
run_parser.set_defaults(action=run_matrix)

quality_parser = subparsers.add_parser('quality', help='compare encoded Opus with aligned source PCM')
quality_parser.add_argument('--binary', type=Path, required=True)
quality_parser.add_argument('--fixture', type=Path, required=True)
quality_parser.add_argument('--output', type=Path, required=True)
quality_parser.add_argument('--label', choices=('before', 'after'), required=True)
quality_parser.set_defaults(action=run_quality)

arguments = parser.parse_args()
arguments.action(arguments)
