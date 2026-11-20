#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
AUVRA_HOST_OUTPUT=$1
mkdir -p "$(dirname -- "$AUVRA_HOST_OUTPUT")"
if [ -f "$AUVRA_HOST_OUTPUT" ] && [ "$AUVRA_HOST_OUTPUT" -nt "$AUVRA_ROOT/platforms/macos/Helpers/SoloistSupervisor.swift" ]; then exit 0; fi
xcrun swiftc -O -swift-version 6 -target arm64-apple-macosx14.0 -parse-as-library \
  "$AUVRA_ROOT/platforms/macos/Helpers/SoloistSupervisor.swift" -o "$AUVRA_HOST_OUTPUT"
codesign --force --sign - "$AUVRA_HOST_OUTPUT"
