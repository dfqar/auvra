#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
python3 "$AUVRA_ROOT/scripts/generate-audio-fixtures.py" --hardware-only
python3 "$AUVRA_ROOT/scripts/generate-library-fixtures.py"
AUVRA_MAC_SMOKE_APP="$AUVRA_ROOT/build/macos-smoke/AuvraMacIntegrationSmoke.app"
mkdir -p "$AUVRA_MAC_SMOKE_APP/Contents/MacOS"
cat > "$AUVRA_MAC_SMOKE_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>app.auvra.integration-smoke</string>
<key>CFBundleExecutable</key><string>AuvraMacIntegrationSmoke</string>
<key>CFBundleName</key><string>Auvra integration check</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
xcrun swiftc -swift-version 6 -target arm64-apple-macosx14.0 -parse-as-library \
  -import-objc-header "$AUVRA_ROOT/include/player.h" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/LibraryModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerEvent.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerCommand.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerBridge.swift" \
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
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/RemoteModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/KeychainCredentialStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyOAuth.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LoopbackOAuthCallback.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyAuthorization.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SoloistRuntime.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LibraryFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalArtworkLoader.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/MediaPlayerSystemSession.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/MacKeyboardShortcuts.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/MacIntegrationSmoke.swift" \
  "$AUVRA_ROOT/target/release/libplayer_ffi.a" -framework CoreAudio -framework AudioToolbox -framework CoreFoundation -framework AVFoundation -framework CoreMedia -framework MediaPlayer -framework ImageIO \
  -o "$AUVRA_MAC_SMOKE_APP/Contents/MacOS/AuvraMacIntegrationSmoke"
codesign --force --deep --sign - "$AUVRA_MAC_SMOKE_APP"
AUVRA_MAC_CHECK_DIR=$(mktemp -d "$AUVRA_ROOT/build/macos-smoke/session.XXXXXX")
exec "$AUVRA_MAC_SMOKE_APP/Contents/MacOS/AuvraMacIntegrationSmoke" "$AUVRA_MAC_CHECK_DIR" "$AUVRA_ROOT/build/library-fixtures" "$@"
