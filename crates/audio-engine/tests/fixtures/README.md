# Generated test audio

These fixtures contain only original signals generated for Auvra tests:
quiet 440 Hz (200 ms stereo 44.1 kHz WAV/FLAC and 100 ms mono 22.05 kHz WAV),
and separate left 1 kHz / right 30 kHz tones (100 ms stereo 96 kHz WAV) for an
anti-aliasing regression. All WAV samples are 16-bit PCM. Apple afconvert encoded
FLAC from stereo-44100.wav. There is no third-party recording here.

Regenerate with `python3 scripts/generate-audio-fixtures.py` on macOS. The stereo
fixture exceeds one encoder packet so afconvert writes a finalized FLAC stream.
Longer quiet hardware-check files live only under ignored build/audio-fixtures.

M3 codec fixtures (`stereo.mp3`, `stereo.ogg`, `stereo-aac.m4a`,
`stereo-alac.m4a`, `stereo.aiff`) encode the same original 200 ms stereo WAV.
Regenerate with `python3 scripts/generate-format-fixtures.py --ffmpeg /path/to/ffmpeg`.
Only encoded test data is retained; the external encoder is not bundled or a
runtime dependency. AAC/MP4 includes encoder padding; gapless AAC is not promised.
