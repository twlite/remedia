"""Generate a redistributable 440 Hz mono PCM fixture without media subprocesses."""
import argparse
import math
import struct
import wave

parser = argparse.ArgumentParser()
parser.add_argument('output')
parser.add_argument('--seconds', type=int, default=15)
args = parser.parse_args()
if args.seconds < 1:
    parser.error('seconds must be positive')
with wave.open(args.output, 'wb') as output:
    output.setparams((1, 2, 48000, 0, 'NONE', 'not compressed'))
    second = b''.join(struct.pack('<h', int(4096 * math.sin(2 * math.pi * 440 * i / 48000))) for i in range(48000))
    for _ in range(args.seconds):
        output.writeframesraw(second)
