#!/bin/sh
# Cross-process startup admission without a player, account or audio output.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT="$ROOT/build/launch-smoke"
mkdir -p "$OUT"
xcrun swiftc -swift-version 6 -target arm64-apple-macosx14.0 -parse-as-library \
    "$ROOT/platforms/macos/Auvra/Services/ApplicationSessionLock.swift" \
    "$ROOT/platforms/macos/Tests/ApplicationSessionLockSmoke.swift" \
    -o "$OUT/ApplicationSessionLockSmoke"
exec "$OUT/ApplicationSessionLockSmoke"
