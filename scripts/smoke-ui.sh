#!/bin/sh
# Deterministic native UI + macOS playback ownership checks; no account, network or real audio.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
python3 - "$AUVRA_ROOT" <<'PY'
import os, pathlib, plistlib, shutil, subprocess, sys
root=pathlib.Path(sys.argv[1]); bundle=root/'build/ui-regression/AuvraUIRegression.app'
contents=bundle/'Contents'; (contents/'MacOS').mkdir(parents=True, exist_ok=True)
(contents/'Resources').mkdir(exist_ok=True)
with (contents/'Info.plist').open('wb') as stream:
    plistlib.dump(dict(CFBundleIdentifier='app.auvra.ui-regression',CFBundleExecutable='AuvraUIRegression',CFBundleName='Auvra UI regression',CFBundlePackageType='APPL',LSMinimumSystemVersion='14.0',NSPrincipalClass='NSApplication'),stream)
assets=root/'build/DerivedData/Build/Products/Release/Auvra.app/Contents/Resources/Assets.car'
if assets.exists(): shutil.copy2(assets, contents/'Resources/Assets.car')
sources=sorted(str(p) for p in (root/'platforms/macos/Auvra').rglob('*.swift') if p.name!='AuvraApp.swift')
command=['xcrun','swiftc','-O','-Xlinker','-dead_strip','-swift-version','6','-target','arm64-apple-macosx14.0','-parse-as-library','-I',str(root/'include'),'-import-objc-header',str(root/'platforms/macos/Auvra/Auvra-Bridging-Header.h')]+sources+[str(root/'platforms/macos/Tests/PlayerUIRegression.swift'),str(root/'platforms/macos/Tests/LyricsRegression.swift'),str(root/'platforms/macos/Tests/ArtistBannerRegression.swift'),str(root/'target/release/libplayer_ffi.a'),str(root/'target/release/libstill_ffi.a')]
for framework in ('CoreAudio','AudioToolbox','CoreFoundation','AVFoundation','CoreMedia','MediaPlayer','ImageIO','Metal','QuartzCore','AppKit','CoreGraphics','Foundation','IOKit','IOSurface'):
    command += ['-framework',framework]
command += ['-lc++','-o',str(contents/'MacOS/AuvraUIRegression')]
subprocess.run(command,check=True)
subprocess.run(['codesign','--force','--deep','--sign','-',str(bundle)],check=True)
PY
AUVRA_UI_EVIDENCE=${AUVRA_UI_EVIDENCE_DIR:-"$AUVRA_ROOT/build/ui-regression/latest"}
mkdir -p "$AUVRA_UI_EVIDENCE"
exec env STILL_PROFILE_WHEN_INACTIVE=1 "$AUVRA_ROOT/build/ui-regression/AuvraUIRegression.app/Contents/MacOS/AuvraUIRegression" "$AUVRA_UI_EVIDENCE"
