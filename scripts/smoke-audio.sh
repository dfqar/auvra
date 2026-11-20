#!/bin/sh
# Explicit real-output check on the current default device, using original quiet tones.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python3 "$AUVRA_ROOT/scripts/generate-audio-fixtures.py" --hardware-only
"$AUVRA_ROOT/scripts/cargo.sh" run --locked --release -p audio-engine --example output_smoke -- "$AUVRA_ROOT/build/audio-fixtures"
exec "$AUVRA_ROOT/scripts/smoke-bridge.sh" "$AUVRA_ROOT/build/audio-fixtures"
