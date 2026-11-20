#!/bin/sh
set -eu
AUVRA_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$AUVRA_ROOT/build/spotify-smoke"
xcrun swiftc -swift-version 6 -target arm64-apple-macosx14.0 -parse-as-library \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/LibraryModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/RemoteModels.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerEvent.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Bridge/PlayerCommand.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/PlatformServices.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/KeychainCredentialStore.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyOAuth.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/LoopbackOAuthCallback.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SpotifyAuthorization.swift" \
  "$AUVRA_ROOT/platforms/macos/Auvra/Services/SoloistRuntime.swift" \
  "$AUVRA_ROOT/platforms/macos/Tests/SpotifyIntegrationSmoke.swift" \
  -o "$AUVRA_ROOT/build/spotify-smoke/spotify-smoke"
"$AUVRA_ROOT/build/spotify-smoke/spotify-smoke"
"$AUVRA_ROOT/scripts/build-spotify-host.sh" "$AUVRA_ROOT/build/spotify-smoke/AuvraPlaybackHost"
exec python3 "$AUVRA_ROOT/scripts/check-spotify-supervisor.py" "$AUVRA_ROOT/build/spotify-smoke/AuvraPlaybackHost"
