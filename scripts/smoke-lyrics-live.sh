#!/bin/sh
# Explicit public metadata lookup. No Spotify credentials, account or audio required.
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
mkdir -p "$AUVRA_ROOT/build/lyrics-verification"
xcrun swiftc -O -swift-version 6 -parse-as-library \
  -import-objc-header "$AUVRA_ROOT/include/player.h" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/LibraryModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerEvent.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerCommand.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/RemoteModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Models/Lyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LyricsService.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/UnisonLyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LrcRedLyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/MusixmatchLyrics.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/MusixmatchPayload.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/LyricsServiceLive.swift" \
  "$AUVRA_ROOT/target/release/libplayer_ffi.a" -framework CoreAudio -framework AudioToolbox -framework CoreFoundation -framework AVFoundation -framework CoreMedia \
  -o "$AUVRA_ROOT/build/lyrics-verification/lyrics-service-live"
exec "$AUVRA_ROOT/build/lyrics-verification/lyrics-service-live" "$@"
