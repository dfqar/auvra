#!/usr/bin/env python3
"""Original quiet, segmented tones for analysis/hardware checks; writes only ignored build/."""
import math
import struct
import wave
from pathlib import Path

root = Path(__file__).resolve().parents[1] / 'build' / 'analysis-fixtures'
root.mkdir(parents=True, exist_ok=True)
rate = 44100
pcm = bytearray()
for frame in range(rate * 16):
    segment = frame // (rate * 4)
    hz = [1000, 0, 120, 4000][segment]
    # The amplitude is intentionally quiet. Each boundary has a short fade to avoid clicks.
    within = frame % (rate * 4)
    fade = min(1, within / 441, (rate * 4 - within - 1) / 441)
    value = round(0.04 * fade * math.sin(2 * math.pi * hz * frame / rate) * 32767) if hz else 0
    pcm.extend(struct.pack('<hh', value, value))
with wave.open(str(root / 'Auvra Analysis Check.wav'), 'wb') as output:
    output.setnchannels(2)
    output.setsampwidth(2)
    output.setframerate(rate)
    output.writeframes(pcm)
print('Generated original quiet 16 s analysis fixture: 1 kHz / silence / 120 Hz / 4 kHz')
