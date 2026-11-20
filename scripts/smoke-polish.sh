#!/bin/sh
# M10 native settings/EQ/persistence/recovery/mini verification using isolated state.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
python3 "$AUVRA_ROOT/scripts/generate-analysis-fixtures.py"
AUVRA_POLISH_APP="$AUVRA_ROOT/build/polish-smoke/AuvraPolishSmoke.app"
mkdir -p "$AUVRA_POLISH_APP/Contents/MacOS"
cat > "$AUVRA_POLISH_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>app.auvra.polish-smoke</string>
<key>CFBundleExecutable</key><string>AuvraPolishSmoke</string>
<key>CFBundleName</key><string>Auvra polish check</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
python3 - "$AUVRA_ROOT" "$AUVRA_POLISH_APP" <<'PYTHON'
import pathlib, subprocess, sys

root, bundle = map(pathlib.Path, sys.argv[1:])
app = root / 'platforms/macos/Auvra'
sources = sorted(str(path) for path in app.rglob('*.swift') if path.name != 'AuvraApp.swift')
command = ['xcrun', 'swiftc', '-O', '-Xlinker', '-dead_strip', '-swift-version', '6',
           '-target', 'arm64-apple-macosx14.0', '-parse-as-library', '-I', str(root / 'include'),
           '-import-objc-header', str(app / 'Auvra-Bridging-Header.h')]
command += sources + [str(root / 'platforms/macos/Tests/PolishSmoke.swift'),
                      str(root / 'target/release/libplayer_ffi.a'),
                      str(root / 'target/release/libstill_ffi.a')]
for framework in ('CoreAudio', 'AudioToolbox', 'CoreFoundation', 'AVFoundation', 'CoreMedia', 'MediaPlayer', 'ImageIO',
                  'Metal', 'QuartzCore', 'AppKit', 'CoreGraphics', 'Foundation', 'IOKit', 'IOSurface'):
    command += ['-framework', framework]
command += ['-lc++', '-o', str(bundle / 'Contents/MacOS/AuvraPolishSmoke')]
subprocess.run(command, check=True)
PYTHON
codesign --force --deep --sign - "$AUVRA_POLISH_APP"
AUVRA_POLISH_EVIDENCE=${AUVRA_POLISH_EVIDENCE_DIR:-"$AUVRA_ROOT/build/polish-smoke/latest"}
mkdir -p "$AUVRA_POLISH_EVIDENCE"
# Own floating test window; allow inactive profiling while preserving hidden/occlusion gates.
exec env STILL_PROFILE_WHEN_INACTIVE=1 "$AUVRA_POLISH_APP/Contents/MacOS/AuvraPolishSmoke" \
  "$AUVRA_ROOT/build/analysis-fixtures/Auvra Analysis Check.wav" "$AUVRA_POLISH_EVIDENCE"
