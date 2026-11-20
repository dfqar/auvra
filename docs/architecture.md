# Architecture

The main window, Now Playing and mini player share a single Swift `PlayerStore`. An action such as Play enters the Rust coordinator as a command. Its request ID lets the host distinguish acceptance, completion and a newer user intent. Provider events and observed playback state update the store and the system media session. Restoring a saved queue leaves playback paused until an explicit Play.

| Area | Responsibility |
| --- | --- |
| [macOS host](../platforms/macos/Auvra) | SwiftUI and AppKit views, media keys and system Now Playing, protected file bookmarks, Keychain, lyrics lookups and artwork IO. |
| [Player core](../crates/player-core) and [domain](../crates/player-domain) | Typed commands and events, cancellation, source capabilities, active transport and queue ownership. The domain has no HTTP, OS audio or GPU dependency. |
| [Local audio engine](../crates/audio-engine) | Decode, resample and apply volume and EQ before presenting local audio to CoreAudio. The callback transfers bounded data to a worker for analysis. |
| [Library](../crates/player-library) and [session](../crates/player-session) | Scan folders selected by the user, index metadata and artwork, persist queues and settings in SQLite. Session IO runs on a separate worker. |
| [Spotify provider](../crates/provider-spotify) and [host helper](../platforms/macos/Helpers) | Handle Web API requests, catalogue and Connect control. Supervise an independently installed Soloist for playback on this Mac. |
| [Lyrics parser](../crates/player-lyrics) | Normalize timed words, lines and plain text into a single document for lyric display and translation. Network source selection stays in the host. |
| [Visual controls](../crates/player-visuals) and [native Frost host](../platforms/macos/Auvra/Still) | Turn accepted features into smoothed controls. Supply the ambient palette and manage Metal surface visibility and lifetime. |

## Audio and ownership

Local analysis runs on presented audio. Its worker publishes compact spectral features tagged with an epoch. Pause, seek and source changes invalidate old features. The audio callback avoids allocations, locks, IO, FFT and Swift calls.

For Spotify, the Web API carries metadata and commands. On macOS 14.2 or later an optional, separate process tap can receive audio from the authenticated Soloist that Auvra owns. The host binds a lease to the runtime, device and media occurrence. Native code independently verifies the process identity, exact parent and signing requirement. Changing source, device or child invalidates the lease. External Connect devices have their own queue and audio output.

The system media session belongs to the Swift host. It may claim an active local source or verified, playing local Soloist. The silent demo, paused restore and external Connect leave system ownership available to other apps. [Spotify setup and boundaries](spotify.md) · [Data and privacy](data-and-privacy.md)

## Rendering

Now Playing, the compact bar and the artist banner share the Frost implementation. Swift submits artwork colours before the first drawable frame. The renderer blends palette changes and accepts short-lived modulation independently from manual scene parameters. Hidden windows suspend continuous rendering. Surface generations keep old callbacks from drawing into a replacement window. [Frost credits and included files](still-import.md)

## ABI and lifetime

The host gives each player or renderer handle one serial owner. Audio samples, Metal textures and GPU buffers stay on their owning side of the boundary. The player bridge carries bounded UTF-8 data and the renderer bridge accepts transient controls. [C interface](ffi.md)
