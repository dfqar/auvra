#!/usr/bin/env python3
"""Add original metadata and a tiny generated PNG to the existing original FLAC tone."""
import struct
import zlib
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]

def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 3, 2, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress((b'\0' + bytes([35, 100, 120]) * 3) * 2)) + chunk(b'IEND', b'')
source = (ROOT / 'crates/audio-engine/tests/fixtures/stereo-44100.flac').read_bytes()
assert source[:4] == b'fLaC'
pos = 4
blocks = []
while True:
    header = source[pos]
    size = int.from_bytes(source[pos + 1:pos + 4], 'big')
    blocks.append((header & 127, source[pos + 4:pos + 4 + size]))
    pos += 4 + size
    if header & 128:
        break
stream_info = next(data for kind, data in blocks if kind == 0)
audio = source[pos:]
picture = struct.pack('>II', 3, 9) + b'image/png' + struct.pack('>I', 0)
picture += struct.pack('>IIIII', 3, 2, 24, 0, len(png)) + png
out = ROOT / 'crates/player-library/tests/fixtures'
out.mkdir(parents=True, exist_ok=True)
for filename, title, number in [('tagged-first.flac', 'First Light', '2'), ('tagged-second.flac', 'Тихий берег', '1')]:
    tags = [f'TITLE={title}', 'ARTIST=Auvra Sessions', 'ALBUM=Quiet Hours', 'ALBUMARTIST=Auvra Sessions', f'TRACKNUMBER={number}', 'DISCNUMBER=1', 'DATE=2026']
    vendor = b'Auvra generated fixture'
    comments = struct.pack('<I', len(vendor)) + vendor + struct.pack('<I', len(tags))
    for tag in tags:
        raw = tag.encode('utf-8')
        comments += struct.pack('<I', len(raw)) + raw
    data = b'fLaC'
    for kind, payload in [(0, stream_info), (4, comments), (134, picture)]:
        data += bytes([kind]) + len(payload).to_bytes(3, 'big') + payload
    (out / filename).write_bytes(data + audio)

# A longer original library for native/bridge playback checks (ignored by Git).
long_source = (ROOT / 'build/audio-fixtures/Auvra Quiet Check.flac').read_bytes()
long_pos = 4
while True:
    header = long_source[long_pos]
    size = int.from_bytes(long_source[long_pos + 1:long_pos + 4], 'big')
    if header & 127 == 0:
        long_info = long_source[long_pos + 4:long_pos + 4 + size]
    long_pos += 4 + size
    if header & 128:
        break
hardware = ROOT / 'build/library-fixtures/Quiet Hours'
hardware.mkdir(parents=True, exist_ok=True)
for filename in ['tagged-first.flac', 'tagged-second.flac']:
    short = (out / filename).read_bytes()
    metadata_end = 4
    while True:
        header = short[metadata_end]
        size = int.from_bytes(short[metadata_end + 1:metadata_end + 4], 'big')
        metadata_end += 4 + size
        if header & 128:
            break
    result = bytearray(short[:metadata_end])
    result[8:42] = long_info
    (hardware / filename).write_bytes(result + long_source[long_pos:])
singles = ROOT / 'build/library-fixtures/Singles'
singles.mkdir(parents=True, exist_ok=True)
(singles / 'Auvra Mono Check.wav').write_bytes((ROOT / 'build/audio-fixtures/Auvra Mono Check.wav').read_bytes())
