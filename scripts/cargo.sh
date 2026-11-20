#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# A tool installation may be reused; no source or build artifacts depend on Still.
AUVRA_TOOLS=${AUVRA_TOOLCHAIN_ROOT:-"$AUVRA_ROOT/.toolchains"}
if [ -x "$AUVRA_TOOLS/cargo/bin/cargo" ]; then
  export CARGO_HOME="$AUVRA_TOOLS/cargo"
  export RUSTUP_HOME="$AUVRA_TOOLS/rustup"
  export PATH="$CARGO_HOME/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust is required. Install the pinned toolchain or set AUVRA_TOOLCHAIN_ROOT to an existing .toolchains directory." >&2
  exit 1
fi
cd "$AUVRA_ROOT"
exec cargo "$@"
