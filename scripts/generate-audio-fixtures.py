#!/usr/bin/env python3
"""Generate original audio fixtures. FLAC encoding requires macOS afconvert."""
import argparse
import math
import struct
import subprocess
import wave
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def write_wav(path, rate, channels, seconds, quiet=True):
    path.parent.mkdir(parents=True, exist_ok=True)
    samples = bytearray()
    for frame in range(int(rate * seconds)):
        for channel in range(channels):
            frequency = 440 if quiet else (1000 if channel == 0 else 30000)
            amplitude = 0.015 if quiet else 0.1
            value = round(math.sin(2 * math.pi * frequency * frame / rate) * amplitude * 32767)
            samples.extend(struct.pack("<h", value))
    with wave.open(str(path), "wb") as output:
        output.setnchannels(channels)
        output.setsampwidth(2)
        output.setframerate(rate)
        output.writeframes(samples)


def encode_flac(wav_path):
    flac_path = wav_path.with_suffix(".flac")
    subprocess.run(["afconvert", "-f", "flac", "-d", "flac", str(wav_path), str(flac_path)], check=True)
    if flac_path.read_bytes()[:4] != b"fLaC":
        raise RuntimeError(f"afconvert did not finalize {flac_path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hardware-only", action="store_true", help="Write only ignored build fixtures")
    args = parser.parse_args()
    if not args.hardware_only:
        fixtures = ROOT / "crates/audio-engine/tests/fixtures"
        # More than one encoder packet: afconvert does not finalize very short FLACs.
        write_wav(fixtures / "stereo-44100.wav", 44100, 2, 0.2)
        write_wav(fixtures / "mono-22050.wav", 22050, 1, 0.1)
        write_wav(fixtures / "stereo-96000.wav", 96000, 2, 0.1, quiet=False)
        encode_flac(fixtures / "stereo-44100.wav")
    hardware = ROOT / "build/audio-fixtures"
    write_wav(hardware / "Auvra Quiet Check.wav", 44100, 2, 8)
    encode_flac(hardware / "Auvra Quiet Check.wav")
    write_wav(hardware / "Auvra Mono Check.wav", 22050, 1, 3)


if __name__ == "__main__":
    main()
