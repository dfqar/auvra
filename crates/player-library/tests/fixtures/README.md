# Original metadata fixtures

These are the original quiet 440 Hz FLAC signal from audio-engine, with two sets
of generated Vorbis comments and an original 3×2 solid PNG embedded as a front
cover. No third-party recording or artwork is included. Regenerate using
`python3 scripts/generate-library-fixtures.py` after the audio fixtures generator.

`tagged.mp3`, `tagged.m4a` (ALAC) and `tagged.ogg` (Vorbis) contain the same
original signal and PNG, plus ID3/MP4/Vorbis metadata. Regenerate with
`python3 scripts/generate-format-fixtures.py --ffmpeg /path/to/ffmpeg`. No encoder
executable is included in Auvra or required to run the tests.
