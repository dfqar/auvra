#!/bin/sh
# Explicit CoreAudio output, followed by the real Swift/ABI analysis projection.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python3 "$AUVRA_ROOT/scripts/generate-analysis-fixtures.py"
python3 "$AUVRA_ROOT/scripts/generate-audio-fixtures.py" --hardware-only
"$AUVRA_ROOT/scripts/cargo.sh" run --locked --release -p audio-engine --example analysis_smoke -- "$AUVRA_ROOT/build/analysis-fixtures/Auvra Analysis Check.wav"
exec "$AUVRA_ROOT/scripts/smoke-bridge.sh" "$AUVRA_ROOT/build/audio-fixtures"
