# What Auvra stores and sends

You can play local files without connecting a Spotify account. Opening online lyrics for a local track can still send its metadata to a lyric provider. Auvra’s in-app summary is under **Settings → Privacy**.

## On your Mac

| Data | Location and use |
| --- | --- |
| Spotify authorization and Soloist API key | macOS Keychain, service `app.auvra.credentials`. The app stores the refresh credential and personal Soloist key there. It uses device-only Keychain accessibility after first unlock. The OAuth access token lives in memory. |
| Spotify playback runtime, session and cache | The signed executable is installed in `~/Library/Application Support/Auvra/PlaybackRuntime/`. Soloist owns its session under `SpotifyPlayback/` in the same Application Support folder and its media cache under `~/Library/Caches/Auvra/SpotifyPlayback/`. |
| Local library | The library index, extracted artwork and read-only macOS bookmarks for files and folders are under `~/Library/Application Support/Auvra/`. Scanning reads the selected files on disk. |
| Playback and preferences | `transport.sqlite3` stores queue and playback position, volume, sound preferences and the account-bound restored Spotify selection. Auvra restores on pause. Theme, device name, public Client ID and account-specific favorite pins use macOS preferences. |
| Lyrics | Explicit imports and matching metadata live in `Lyrics/imports.json`, bounded to 32 entries and 4 MiB. Online responses and the current translation use memory. Auvra leaves the original import file in place. |

**Soloist key visibility.** Auvra sends the key to its playback helper through a private pipe. The official executable takes the key in a command-line argument. Process listings, crash dumps and raw runtime logs may expose it. When reporting an issue, share the symptom, macOS version and output type. Remove credentials, callback URLs, listening history and personal paths from any supporting material. [Playback helper](../platforms/macos/Helpers/SoloistSupervisor.swift) · [Reporting issues](../CONTRIBUTING.md)

## Network requests

| Destination | Data and trigger |
| --- | --- |
| Spotify authorization, Web API and image CDN | The browser completes sign-in. Auvra exchanges the authorization code and refreshes the credential, then requests metadata, library and playback state. Actions you take can send playback commands, library and playlist edits or a selected playlist cover. Artwork requests go to supported Spotify CDN hosts. The official Soloist binary also makes its own service connections. |
| LRCLIB | Opening Lyrics can send the track title, primary artist, album when available and duration. A fallback search uses title and artist. |
| AMLL TTML DB on GitHub | For a valid Spotify track ID, Auvra can request that ID’s TTML file from the public database. |
| Unison and lrc.red | Fallback lookups send title and primary artist, then the matched version ID or ISRC. lrc.red is active only when a project URL and contact email have been configured. They appear in its request header. |
| Musixmatch | An optional experiment, off by default, can send title, artist, album, duration and Spotify track ID when available. Its client token stays in memory. Public use requires a separate review. [Release scope](public-release.md) |

Lyrics lookup starts when the panel opens or the selected recording changes while the panel is open. Matching local imports take priority. Auvra uses an ephemeral session for lyric requests and keeps Spotify credentials out of them. Each service receives the connecting IP address. Translation starts on demand and runs locally. Apple may download language packs. An optional local translation helper reads separately prepared models. [Lyrics service](../platforms/macos/Auvra/Services/LyricsService.swift) · [Optional model pack](../LICENSES/offline-lyrics/README.md)

## Audio access

Auvra analyzes local audio after its equalizer and volume controls. On macOS 14.2 and later, music-responsive lighting and the optional experimental spatial stereo path can request macOS system-audio permission for Auvra’s own signed, authenticated Soloist child. Auvra verifies the parent process and playback session and scopes the tap to that child. Audio samples stay in bounded memory buffers. The tap excludes the microphone, other apps and external Connect outputs. The visual renderer receives compact features such as bass energy.

You can turn off music-responsive lighting and experimental Spotify spatial stereo in Auvra Settings, or change system-audio permission in macOS System Settings. If permission is denied, direct Soloist playback and artwork-colored lighting remain available. [Audio capture implementation](../crates/audio-engine/src/owned_output/capture.rs)

## Disconnect and delete data

**Settings → Account → Disconnect** stops the managed player and attempts to remove both Keychain records and the saved Soloist session. Account shows an error if removal fails. The installed runtime, its cache, your Spotify library, your files, local index, pins, preferences and imported lyrics persist. Auvra has no automatic purge interval for these records.

Removing the app bundle leaves its Application Support, Caches, preferences and Keychain entries on the Mac. Quit Auvra before manually changing its SQLite files or removing its local data. Revoking the app on Spotify is a separate account action. Auvra includes no automatic analytics or crash uploader. [Manage Spotify apps](https://www.spotify.com/account/apps/)
