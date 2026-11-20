#!/usr/bin/env python3
"""Original quiet signals for M9. Writes only ignored build/, no user audio."""
import math
import struct
import wave
from pathlib import Path

root = Path(__file__).resolve().parents[1] / 'build' / 'modulation-fixtures'
root.mkdir(parents=True, exist_ok=True)
rate = 44100
pcm = bytearray()
for frame in range(rate * 24):
    t = frame / rate
    if t < 16:
        segment = int(t // 4)
        hz = [1000, 0, 120, 4000][segment]
        within = frame % (rate * 4)
        fade = min(1, within / 441, (rate * 4 - within - 1) / 441)
    else:
        hz = 120
        pulse = (t - 16) % 0.5
        fade = max(0, min(1, pulse / 0.006, (0.12 - pulse) / 0.006))
    value = round(0.04 * fade * math.sin(2 * math.pi * hz * t) * 32767) if hz else 0
    pcm.extend(struct.pack('<hh', value, value))
with wave.open(str(root / 'Auvra Modulation Check.wav'), 'wb') as output:
    output.setnchannels(2)
    output.setsampwidth(2)
    output.setframerate(rate)
    output.writeframes(pcm)
print('Generated original quiet 24 s modulation fixture: 1 kHz / silence / 120 Hz / 4 kHz / bass pulses')
