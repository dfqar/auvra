#!/usr/bin/env python3
"""Original longer tagged tones for native Control Center / mini-player checks (build only)."""
from pathlib import Path
import runpy
import struct

ROOT = Path(__file__).resolve().parents[1]
helpers = runpy.run_path(str(ROOT / 'scripts/generate-audio-fixtures.py'))
source = ROOT / 'build/macos-media-source.wav'
helpers['write_wav'](source, 44100, 2, 60)
helpers['encode_flac'](source)

def split_flac(data):
    assert data[:4] == b'fLaC'
    blocks = []
    pos = 4
    while True:
        header = data[pos]
        size = int.from_bytes(data[pos + 1:pos + 4], 'big')
        blocks.append((header & 127, data[pos + 4:pos + 4 + size]))
        pos += 4 + size
        if header & 128:
            return blocks, data[pos:]

blocks, audio = split_flac(source.with_suffix('.flac').read_bytes())
stream_info = next(data for kind, data in blocks if kind == 0)
cover_blocks, _ = split_flac((ROOT / 'crates/player-library/tests/fixtures/tagged-first.flac').read_bytes())
picture = next(data for kind, data in cover_blocks if kind == 6)
output = ROOT / 'build/macos-fixtures'
output.mkdir(parents=True, exist_ok=True)
for index, title in enumerate(['Media Keys First', 'Media Keys Second'], start=1):
    tags = [f'TITLE={title}', 'ARTIST=Auvra Sessions', 'ALBUM=Mac Integration',
            'ALBUMARTIST=Auvra Sessions', f'TRACKNUMBER={index}', 'DISCNUMBER=1', 'DATE=2026']
    vendor = b'Auvra original macOS check'
    comments = struct.pack('<I', len(vendor)) + vendor + struct.pack('<I', len(tags))
    for tag in tags:
        raw = tag.encode()
        comments += struct.pack('<I', len(raw)) + raw
    encoded = b'fLaC'
    for kind, data in [(0, stream_info), (4, comments), (134, picture)]:
        encoded += bytes([kind]) + len(data).to_bytes(3, 'big') + data
    (output / f'{index:02d}.flac').write_bytes(encoded + audio)
print('Created two original 60-second quiet FLACs with metadata and generated cover in build/macos-fixtures.')
