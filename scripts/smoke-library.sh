#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$AUVRA_ROOT/scripts/build-rust.sh"
python3 "$AUVRA_ROOT/scripts/generate-audio-fixtures.py" --hardware-only
python3 "$AUVRA_ROOT/scripts/generate-library-fixtures.py"
mkdir -p "$AUVRA_ROOT/build/library-smoke"
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
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/RemoteModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/KeychainCredentialStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyOAuth.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LoopbackOAuthCallback.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyAuthorization.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SoloistRuntime.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LibraryFileAccess.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LocalArtworkLoader.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/LibraryBridgeSmoke.swift" \
  "$AUVRA_ROOT/target/release/libplayer_ffi.a" -framework CoreAudio -framework AudioToolbox -framework CoreFoundation -framework AVFoundation -framework CoreMedia -framework ImageIO \
  -o "$AUVRA_ROOT/build/library-smoke/library-smoke"
AUVRA_LIBRARY_CHECK_DIR=$(mktemp -d "$AUVRA_ROOT/build/library-smoke/session.XXXXXX")
for AUVRA_LIBRARY_PHASE in import restart lost-access; do
  "$AUVRA_ROOT/build/library-smoke/library-smoke" "$AUVRA_LIBRARY_PHASE" "$AUVRA_LIBRARY_CHECK_DIR" "$AUVRA_ROOT/build/library-fixtures"
done
"$AUVRA_ROOT/build/library-smoke/library-smoke" pages "$AUVRA_LIBRARY_CHECK_DIR/paged-session" "$AUVRA_ROOT/build/library-fixtures" "$AUVRA_ROOT/crates/player-library/tests/fixtures/tagged-first.flac"
"$AUVRA_ROOT/scripts/cargo.sh" run --locked --release -p audio-engine --example codec_output_smoke -- \
  "$AUVRA_ROOT/crates/audio-engine/tests/fixtures/stereo.mp3" \
  "$AUVRA_ROOT/crates/audio-engine/tests/fixtures/stereo.ogg" \
  "$AUVRA_ROOT/crates/audio-engine/tests/fixtures/stereo-aac.m4a" \
  "$AUVRA_ROOT/crates/audio-engine/tests/fixtures/stereo-alac.m4a" \
  "$AUVRA_ROOT/crates/audio-engine/tests/fixtures/stereo.aiff"
