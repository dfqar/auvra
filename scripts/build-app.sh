#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec "$AUVRA_ROOT/scripts/cargo.sh" xtask package
