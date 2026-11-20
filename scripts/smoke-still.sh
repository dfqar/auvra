#!/bin/sh
# Own native windows and real Metal Present. Exit 2 is inconclusive OS occlusion,
# recorded separately from injected-signal coverage; never treat it as a full pass.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
AUVRA_STILL_SMOKE_TAG=${AUVRA_STILL_SMOKE_TAG:-}
case "$AUVRA_STILL_SMOKE_TAG" in *[!a-z0-9-]*) printf '%s\n' 'Smoke tag must contain only lowercase letters, digits and hyphens' >&2; exit 2;; esac
AUVRA_STILL_SMOKE_APP="$AUVRA_ROOT/build/still-smoke/AuvraStillSmoke${AUVRA_STILL_SMOKE_TAG:+-$AUVRA_STILL_SMOKE_TAG}.app"
mkdir -p "$AUVRA_STILL_SMOKE_APP/Contents/MacOS"
cat > "$AUVRA_STILL_SMOKE_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>app.auvra.still-smoke</string>
<key>CFBundleExecutable</key><string>AuvraStillSmoke</string>
<key>CFBundleName</key><string>Auvra Still integration check</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
if [ -n "$AUVRA_STILL_SMOKE_TAG" ]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier app.auvra.still-smoke.$AUVRA_STILL_SMOKE_TAG" "$AUVRA_STILL_SMOKE_APP/Contents/Info.plist"
fi
xcrun swiftc -swift-version 6 -target arm64-apple-macosx14.0 -parse-as-library \
  -import-objc-header "$AUVRA_ROOT/include/still.h" \
  "$AUVRA_ROOT/platforms/macos/Auvra/App/AppAppearance.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Still/EngineController.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Still/MetalHostView.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Still/StillView.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/StillHostSmoke.swift" \
  "$AUVRA_ROOT/target/release/libstill_ffi.a" \
  -framework Metal -framework QuartzCore -framework AppKit -framework CoreGraphics -framework Foundation -framework IOKit -framework IOSurface -lc++ \
  -o "$AUVRA_STILL_SMOKE_APP/Contents/MacOS/AuvraStillSmoke"
codesign --force --deep --sign - "$AUVRA_STILL_SMOKE_APP"
AUVRA_STILL_CHECK_DIR=${AUVRA_STILL_EVIDENCE_DIR:-"$AUVRA_ROOT/build/still-smoke/latest"}
mkdir -p "$AUVRA_STILL_CHECK_DIR"
exec "$AUVRA_STILL_SMOKE_APP/Contents/MacOS/AuvraStillSmoke" "$AUVRA_STILL_CHECK_DIR"
