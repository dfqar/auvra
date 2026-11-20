# Spotify features in Auvra

Use this table to see what Auvra requests and where the result appears. Availability depends on your Web API registration, account permissions, market, entitlement and the selected playback device. [Connection instructions](spotify.md) · [Spotify Web API reference](https://developer.spotify.com/documentation/web-api)

| In Auvra | What it uses |
| --- | --- |
| Search | Search tracks, artists, albums, playlists, shows, episodes and audiobooks together or by type. A close artist match can lead the mixed results. A pasted Spotify link or URI can open an individual item, including a chapter. |
| Tracks, albums and artists | Track metadata links to the parent album and artists. Albums show tracks and artists show releases. When the artist top-tracks endpoint is unavailable, **Selected tracks** uses search results matched to that artist’s ID. |
| Your Library | Liked Songs, playlists, saved albums, shows, episodes and audiobooks, followed artists, recent plays and personal top items. Save, remove, follow and check membership where the account allows it. The favorite playlist pin is local and account-specific. |
| Home | Recent plays, top tracks and artists, library picks, albums from favorite artists, radio playlist searches and new-album searches. These shelves use data available to Auvra. |
| Playlists | Create playlists, edit metadata and visibility, add, remove, move or replace items, and upload a JPEG cover. Owner and collaborator permissions differ. Positional edits use the playlist snapshot or recheck the selected row where appropriate. |
| Podcasts and books | Open a show or book, browse episodes or chapters, and start available items. With the optional playback-position permission, **Continue from** uses the saved Spotify position. |
| Connect | Read devices, select an output, observe playback, play, pause, seek, skip, change volume, shuffle or repeat, and view or add queue items. The selected device advertises the controls it supports. |
| Playback on this Mac | The signed Soloist process supplies local playback state, position and queue events. Auvra can send supported transport actions to its paired output and publish that session to macOS. Playback on an external device stays outside the local system media session. |

## Discovery and continuation

Home combines the account’s listening data with catalog results. Spotify’s official Home feed and AI DJ are unavailable through this integration. Access to other people's playlists, editorial selections and personalized mixes depends on what Spotify returns for the registration. A Spotify playlist found in search may expose metadata while its track list is unavailable. [Spotify’s development-mode changes](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)

On Auvra’s own Soloist output, track continuation can select candidates using catalog metadata and append a next item when the queue is ending. It takes account of repeat mode, the observed queue and device capabilities. Spotify controls audio delivery and buffering. External Connect devices manage their own continuation. [Selection logic](../crates/provider-spotify/src/autoplay.rs)

Lyrics lookup uses separate sources or an imported file. Spotify audio and downloading tracks for offline use are outside the Web API implementation. [Data flows](data-and-privacy.md) · [Third-party service terms](public-release.md)
