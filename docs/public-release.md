# Source publication and distribution

This source edition of Auvra is for building on an Apple Silicon Mac. Contributors can build the player and use their own local audio without service credentials. The author is **dfqar lab**. Copies and substantial portions of Auvra must retain the copyright and permission notice. [Auvra license](../LICENSE)

## What travels with the source

| Material | Terms and origin |
| --- | --- |
| Original player code, documentation and the README demonstration image | Auvra's MIT notice. The demo uses original artwork and lyric text. |
| The engine code used to render Frost | Imported from Still 2.2.0 under its own MIT notice. Changes and boundaries are documented. [Import notes](still-import.md) · [Still notice](../LICENSES/Still-MIT.txt) |
| Rust dependencies | Each dependency retains its own license. The archive builder reads the locked graph, verifies notices and bundles corresponding Symphonia sources for MPL-covered crates. [Packaging details](../LICENSES/README.md) |
| Optional offline translation models and runtime | The repository contains a pinned manifest, notices and an opt-in pack builder. Model assets are downloaded separately. [Model sources and terms](../LICENSES/offline-lyrics/README.md) |
| Spotify Soloist | Each listener obtains the signed runtime and their own key from Spotify. Spotify prohibits redistributing the runtime. [Access and distribution](https://developer.spotify.com/documentation/soloist#access-and-distribution) |

Songs, catalogue artwork, fetched lyrics and service trademarks belong to their respective rightsholders. The MIT grant covers the project's own contributions.

## Before offering a connected player to the public

- **Spotify access.** A user-supplied Web API Client ID and Premium account provide development access subject to the application's allowlist and available endpoints. A public service using one app registration needs a separate access review. [Quota modes](https://developer.spotify.com/documentation/web-api/concepts/quota-modes)
- **Attribution.** Spotify requires its branding alongside Spotify metadata, artwork and playback, with applicable links back to the service. The current player has no bundled Spotify mark. Its playing and catalogue views still need a branding pass for a public Spotify-enabled product. [Design and branding guidelines](https://developer.spotify.com/documentation/design)
- **Playback and visuals.** Spotify's policy restricts replacing its core experience, analysing Spotify content and synchronising recordings with visual media. Auvra's Spotify playback interface, owned-output lighting and timed-lyrics presentation need review against those restrictions before offering them to other listeners. An end user's macOS audio permission handles device access only. [Spotify Developer Policy](https://developer.spotify.com/policy)
- **Lyrics.** Network access and a provider response do not establish a right to display, translate or redistribute lyrics. The optional Musixmatch implementation uses a mobile client endpoint and is disabled by default. A public integration requires appropriate provider approval. Imported files and original fixtures can be used independently. [Data flows](data-and-privacy.md)
- **macOS downloads.** A local build can be ad-hoc signed. A downloadable release should be signed with Developer ID Application, notarized and tested after a browser download on another Mac. [Archive guide](releases/macos-archive.md)
