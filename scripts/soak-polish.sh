#!/bin/sh
# M10 real CoreAudio/EQ + native Metal performance soak; isolated state, original quiet audio.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
python3 "$AUVRA_ROOT/scripts/generate-modulation-fixtures.py"
AUVRA_SOAK_APP="$AUVRA_ROOT/build/polish-soak/AuvraPolishSoak.app"
mkdir -p "$AUVRA_SOAK_APP/Contents/MacOS"
cat > "$AUVRA_SOAK_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>app.auvra.polish-soak</string>
<key>CFBundleExecutable</key><string>AuvraPolishSoak</string>
<key>CFBundleName</key><string>Auvra polish soak</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
xcrun swiftc -O -Xlinker -dead_strip -swift-version 6 -target arm64-apple-macosx14.0 -parse-as-library \
  -I "$AUVRA_ROOT/include" -import-objc-header "$AUVRA_ROOT/platforms/macos/Auvra/Auvra-Bridging-Header.h" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/LibraryModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerEvent.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerCommand.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerBridge.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/RemoteModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Models/PlayerStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyPreferences.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Models/Lyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Models/LyricsStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LyricsService.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/UnisonLyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LrcRedLyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/MusixmatchLyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/MusixmatchPayload.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/PlatformServices.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalArtworkLoader.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/KeychainCredentialStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyOAuth.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LoopbackOAuthCallback.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyAuthorization.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SoloistRuntime.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LibraryFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/App/AppAppearance.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Still/EngineController.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Still/MetalHostView.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Still/StillView.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/PolishSoak.swift" \
  "$AUVRA_ROOT/target/release/libplayer_ffi.a" "$AUVRA_ROOT/target/release/libstill_ffi.a" \
  -framework CoreAudio -framework AudioToolbox -framework CoreFoundation -framework AVFoundation -framework CoreMedia -framework ImageIO \
  -framework Metal -framework QuartzCore -framework AppKit -framework CoreGraphics -framework Foundation -framework IOKit -framework IOSurface -lc++ \
  -o "$AUVRA_SOAK_APP/Contents/MacOS/AuvraPolishSoak"
codesign --force --deep --sign - "$AUVRA_SOAK_APP"
AUVRA_SOAK_EVIDENCE=${AUVRA_SOAK_EVIDENCE_DIR:-"$AUVRA_ROOT/build/polish-soak/latest"}
mkdir -p "$AUVRA_SOAK_EVIDENCE"
# Own floating test window; allow inactive profiling while preserving hidden/occlusion gates.
exec env STILL_PROFILE_WHEN_INACTIVE=1 "$AUVRA_SOAK_APP/Contents/MacOS/AuvraPolishSoak" \
  "$AUVRA_ROOT/build/modulation-fixtures/Auvra Modulation Check.wav" "$AUVRA_SOAK_EVIDENCE" "${AUVRA_SOAK_SECONDS:-1800}"
