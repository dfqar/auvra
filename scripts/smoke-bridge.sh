#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
mkdir -p "$AUVRA_ROOT/build/bridge-smoke"
xcrun swiftc -swift-version 6 -parse-as-library \
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
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalArtworkLoader.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/RemoteModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/KeychainCredentialStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyOAuth.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LoopbackOAuthCallback.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyAuthorization.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SoloistRuntime.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LibraryFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Models/PlaybackScrubber.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/PlayerBridgeSmoke.swift" \
  "$AUVRA_ROOT/target/release/libplayer_ffi.a" -framework CoreAudio -framework AudioToolbox -framework CoreFoundation -framework AVFoundation -framework CoreMedia \
  -o "$AUVRA_ROOT/build/bridge-smoke/player-bridge-smoke"
exec "$AUVRA_ROOT/build/bridge-smoke/player-bridge-smoke" "$@"
