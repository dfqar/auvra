#!/usr/bin/env python3
"""Regenerate original codec fixtures with an explicitly supplied external ffmpeg encoder."""
import argparse
import shutil
import subprocess
from pathlib import Path
root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--ffmpeg', default=shutil.which('ffmpeg'))
args = parser.parse_args()
if not args.ffmpeg:
    parser.error('Supply --ffmpeg /path/to/ffmpeg; the encoder is not bundled with Auvra')
fixtures = root / 'crates/audio-engine/tests/fixtures'
source = fixtures / 'stereo-44100.wav'
for name, codec in [('stereo.mp3', 'libmp3lame'), ('stereo.ogg', 'libvorbis'), ('stereo-aac.m4a', 'aac'), ('stereo-alac.m4a', 'alac'), ('stereo.aiff', 'pcm_s16be')]:
    subprocess.run([args.ffmpeg, '-hide_banner', '-loglevel', 'error', '-y', '-i', str(source), '-map_metadata', '-1', '-c:a', codec, str(fixtures / name)], check=True)

# Common library tag/artwork formats, derived from the same original signal and PNG.
import base64
import tempfile
library = root / 'crates/player-library/tests/fixtures'
tagged_flac = (library / 'tagged-first.flac').read_bytes()
pos = 4
picture = None
while True:
    kind = tagged_flac[pos]
    size = int.from_bytes(tagged_flac[pos + 1:pos + 4], 'big')
    if kind & 127 == 6:
        picture = tagged_flac[pos + 4:pos + 4 + size]
    pos += 4 + size
    if kind & 128:
        break
assert picture
start = picture.index(b'\x89PNG\r\n\x1a\n')
png = picture[start:picture.index(b'IEND', start) + 8]
with tempfile.TemporaryDirectory(prefix='auvra-format-fixtures-') as temporary:
    cover = Path(temporary) / 'cover.png'
    cover.write_bytes(png)
    for name, codec in [('tagged.mp3', 'libmp3lame'), ('tagged.m4a', 'alac'), ('tagged.ogg', 'libvorbis')]:
        command = [args.ffmpeg, '-hide_banner', '-loglevel', 'error', '-y', '-i', str(source)]
        if name.endswith('.ogg'):
            command += ['-map', '0:a', '-metadata', 'METADATA_BLOCK_PICTURE=' + base64.b64encode(picture).decode()]
        else:
            command += ['-i', str(cover), '-map', '0:a', '-map', '1:v', '-c:v', 'copy', '-disposition:v', 'attached_pic']
        for key, value in [('title', 'Metadata Check'), ('artist', 'Auvra Sessions'), ('album', 'Metadata Checks'), ('album_artist', 'Auvra Sessions'), ('track', '3'), ('disc', '2'), ('date', '2026')]:
            command += ['-metadata', key + '=' + value]
        command += ['-c:a', codec, str(library / name)]
        subprocess.run(command, check=True)
