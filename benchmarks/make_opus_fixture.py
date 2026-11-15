"""Rewrite an Ogg Opus file so it is eligible for Remedia's packet passthrough.

Remedia only forwards Opus packets without trimming, so streams advertising a
pre-skip (every libopus/ffmpeg encode does) take the PCM path. This tool copies
the audio packets of an existing Ogg Opus file into a new file with pre-skip 0,
one packet per 20 ms page and exact granule positions. It uses no media
subprocesses; encode the source material with any tool beforehand.
"""
import argparse
import struct


def crc_table():
    table = []
    for i in range(256):
        r = i << 24
        for _ in range(8):
            r = ((r << 1) ^ 0x04C11DB7) if r & 0x80000000 else (r << 1)
        table.append(r & 0xFFFFFFFF)
    return table


TABLE = crc_table()


def crc(data):
    value = 0
    for byte in data:
        value = ((value << 8) & 0xFFFFFFFF) ^ TABLE[((value >> 24) & 0xFF) ^ byte]
    return value


def page(serial, sequence, granule, flags, packet):
    segments = []
    remaining = len(packet)
    while remaining >= 255:
        segments.append(255)
        remaining -= 255
    segments.append(remaining)
    header = b'OggS' + struct.pack('<BBqIIIB', 0, flags, granule, serial, sequence, 0, len(segments)) + bytes(segments)
    body = header + packet
    checksum = crc(body)
    return body[:22] + struct.pack('<I', checksum) + body[26:]


def read_packets(data):
    packets, current, offset = [], b'', 0
    while offset < len(data):
        assert data[offset:offset + 4] == b'OggS', 'not an Ogg file'
        count = data[offset + 26]
        table = data[offset + 27:offset + 27 + count]
        body = offset + 27 + count
        for lacing in table:
            current += data[body:body + lacing]
            body += lacing
            if lacing < 255:
                packets.append(current)
                current = b''
        offset = body
    return packets


parser = argparse.ArgumentParser()
parser.add_argument('input')
parser.add_argument('output')
parser.add_argument('--loops', type=int, default=1, help='repeat the audio packets this many times')
parser.add_argument('--pre-skip', type=int, default=0, help='pre-skip to write into the OpusHead (default 0)')
args = parser.parse_args()

packets = read_packets(open(args.input, 'rb').read())
head, audio = bytearray(packets[0]), packets[2:]
assert head[:8] == b'OpusHead'
struct.pack_into('<H', head, 10, args.pre_skip)
serial = 0x52454D44
out = bytearray(page(serial, 0, 0, 0x02, bytes(head)))
out += page(serial, 1, 0, 0, b'OpusTags' + struct.pack('<I', 7) + b'remedia' + struct.pack('<I', 0))
samples = 0
sequence = 2
audio = audio * args.loops
for index, packet in enumerate(audio):
    toc = packet[0]
    config, code = toc >> 3, toc & 3
    if config < 12:
        frame_ms = [10, 20, 40, 60][config & 3]
    elif config < 16:
        frame_ms = [10, 20][config & 1]
    else:
        frame_ms = [2.5, 5, 10, 20][config & 3]
    frames = 1 if code == 0 else 2 if code in (1, 2) else (packet[1] & 63)
    samples += int(frame_ms * 48 * frames)
    last = index == len(audio) - 1
    out += page(serial, sequence, samples, 0x04 if last else 0, packet)
    sequence += 1
open(args.output, 'wb').write(out)
print(f'{len(audio)} packets, {samples / 48000:.2f} s')
