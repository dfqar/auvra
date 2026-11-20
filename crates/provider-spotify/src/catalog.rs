use crate::{
    api::{Api, ApiFailure},
    mapping,
};
use player_domain::*;
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct EditContent {
    pub collection: Option<RemoteCollection>,
    pub saved: Vec<RemoteSavedItem>,
}
pub(crate) struct PlaylistPageGuard {
    id: String,
    query_id: u64,
    generation: u64,
    snapshot: String,
    total: u64,
    positions: Vec<u32>,
}
pub(crate) struct ContextPageGuard {
    pub context: String,
    pub query_id: u64,
    pub snapshot: Option<String>,
    pub rows: Vec<(u32, MediaId)>,
}
pub(crate) fn context_page_changed() -> PlayerError {
    PlayerError::new(
        ErrorCode::CatalogChanged,
        "This collection changed or its page expired. Reload it before starting this row.",
    )
}
fn playlist_changed() -> ApiFailure {
    ApiFailure {
        error: PlayerError::new(
            ErrorCode::CatalogChanged,
            "This playlist changed or its page expired. Reload it before moving an item.",
        ),
        status: 409,
        retry_after: None,
        previous_response_timeout: false,
    }
}
fn catalog_failure(mut failure: ApiFailure, kind: &RemoteCatalogKind) -> ApiFailure {
    if matches!(kind, RemoteCatalogKind::ArtistTopTracks) && matches!(failure.status, 403 | 404) {
        failure.error = PlayerError::new(
            ErrorCode::UnsupportedCapability,
            "Spotify’s artist chart is unavailable for this account.",
        );
        return failure;
    }
    if failure.status == 403 {
        failure.error.message = if matches!(kind, RemoteCatalogKind::PlaylistItems) {
            "Spotify has not granted access to this playlist’s tracks. You can still try playing the playlist or open it in Spotify. Only owned or collaborative playlists can be read in Development Mode.".into()
        } else {
            "Spotify has not granted access to this section. Reconnect in Account to grant the required permissions; availability also depends on your account and region.".into()
        };
    }
    failure
}

fn normalize_chapter(row: &mut Value, expected_id: Option<&str>) -> bool {
    let Some(id) = row["id"].as_str().filter(|id| mapping::valid_id(id)) else {
        return false;
    };
    if expected_id.is_some_and(|expected| id != expected) {
        return false;
    }
    let uri_matches = |kind| row["uri"].as_str().and_then(|uri| uri.strip_prefix(kind)) == Some(id);
    let kind = match row.get("type") {
        None | Some(Value::Null) => "chapter",
        Some(Value::String(kind)) => kind.as_str(),
        _ => return false,
    };
    let valid = match kind {
        "chapter" => {
            row["uri"].is_null()
                || uri_matches("spotify:chapter:")
                || uri_matches("spotify:episode:")
        }
        // Spotify documents an episode type and URI for audiobook chapters.
        "episode" => {
            uri_matches("spotify:episode:")
                && (!row["show"].is_object()
                    || row["audiobook"].is_object()
                    || row["chapter_number"].is_u64())
        }
        _ => false,
    };
    if valid {
        row["type"] = json!("chapter");
    }
    valid
}

fn record_spoken_progress(progress: &mut Vec<SpokenProgress>, row: &Value, item: &MediaItem) {
    let point = &row["resume_point"];
    let (Some(position_ms), Some(fully_played)) = (
        point["resume_position_ms"].as_u64(),
        point["fully_played"].as_bool(),
    ) else {
        return;
    };
    if item.duration_ms == 0
        || position_ms > item.duration_ms
        || (!fully_played && (position_ms == 0 || position_ms == item.duration_ms))
        || progress.iter().any(|point| point.media_id == item.id)
    {
        return;
    }
    progress.push(SpokenProgress {
        media_id: item.id.clone(),
        position_ms,
        fully_played,
    });
}

fn recent_play(entry: &Value, item_index: u32) -> Option<RecentPlay> {
    let played_at = entry["played_at"]
        .as_str()
        .filter(|time| {
            (20..=40).contains(&time.len())
                && time.as_bytes().get(10) == Some(&b'T')
                && time
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || b"-:TZ.+".contains(&byte))
        })
        .map(str::to_owned);
    let context_uri = entry["context"]["uri"]
        .as_str()
        .filter(|uri| {
            uri.strip_prefix("spotify:")
                .and_then(|rest| rest.split_once(':'))
                .is_some_and(|(kind, id)| {
                    matches!(kind, "album" | "artist" | "playlist" | "show")
                        && mapping::valid_id(id)
                })
        })
        .map(str::to_owned);
    (played_at.is_some() || context_uri.is_some()).then_some(RecentPlay {
        item_index,
        played_at,
        context_uri,
    })
}

impl Api {
    fn move_snapshot(
        &mut self,
        id: &str,
        query_id: u64,
        from: u32,
        to: u32,
    ) -> Result<String, ApiFailure> {
        // Consume before IO: an uncertain write must never reuse the old page.
        let guard = self.playlist_page.take().ok_or_else(playlist_changed)?;
        if !self.catalog_intent_matches(query_id)
            || guard.id != id
            || guard.query_id != query_id
            || guard.generation != self.generation
            || !guard.positions.contains(&from)
            || u64::from(to) > guard.total
            || from == to
            || from.checked_add(1) == Some(to)
        {
            return Err(playlist_changed());
        }
        if self.playlist_snapshot(id, None)?.as_deref() != Some(guard.snapshot.as_str()) {
            return Err(playlist_changed());
        }
        Ok(guard.snapshot)
    }
    pub fn catalog(&mut self, query: &RemoteQuery) -> Result<RemotePage, ApiFailure> {
        use RemoteCatalogKind::*;
        if !query.kind.is_detail() {
            self.playlist_page = None;
            self.clear_context_page(query.query_id);
        }
        if matches!(query.kind, SearchAll) {
            return self.mixed_search(query);
        }
        // Read the version BEFORE the rows. Checking the same version again at
        // edit time also catches changes made while this page was loading.
        let snapshot = if matches!(query.kind, PlaylistItems) {
            self.playlist_snapshot(&query.text, None)
                .map_err(|failure| catalog_failure(failure, &query.kind))?
        } else {
            None
        };
        let mut params = vec![("limit", "10".into()), ("offset", query.offset.to_string())];
        let (path, wrapper, collection_kind, singleton) = match query.kind {
            SearchAll => unreachable!("mixed search has multiple result wrappers"),
            Search | SearchAlbums | SearchArtists | SearchPlaylists | SearchShows
            | SearchEpisodes | SearchAudiobooks | HomeRadio => {
                let (entity, key, kind) = match query.kind {
                    SearchAlbums => ("album", "albums", Some(RemoteEntityKind::Album)),
                    SearchArtists => ("artist", "artists", Some(RemoteEntityKind::Artist)),
                    SearchPlaylists | HomeRadio => {
                        ("playlist", "playlists", Some(RemoteEntityKind::Playlist))
                    }
                    SearchShows => ("show", "shows", Some(RemoteEntityKind::Show)),
                    SearchEpisodes => ("episode", "episodes", None),
                    SearchAudiobooks => {
                        ("audiobook", "audiobooks", Some(RemoteEntityKind::Audiobook))
                    }
                    _ => ("track", "tracks", None),
                };
                params.push((
                    "q",
                    if matches!(query.kind, HomeRadio) {
                        "radio mix".into()
                    } else {
                        query.text.clone()
                    },
                ));
                params.push(("type", entity.into()));
                ("search".into(), key, kind, false)
            }
            SavedTracks => ("me/tracks".into(), "", None, false),
            SavedEpisodes => ("me/episodes".into(), "", None, false),
            SavedAlbums => ("me/albums".into(), "", Some(RemoteEntityKind::Album), false),
            SavedShows => ("me/shows".into(), "", Some(RemoteEntityKind::Show), false),
            SavedAudiobooks => (
                "me/audiobooks".into(),
                "",
                Some(RemoteEntityKind::Audiobook),
                false,
            ),
            Playlists | HomePlaylists => (
                "me/playlists".into(),
                "",
                Some(RemoteEntityKind::Playlist),
                false,
            ),
            PlaylistInfo => (
                format!("playlists/{}", query.text),
                "",
                Some(RemoteEntityKind::Playlist),
                true,
            ),
            PlaylistImages => (
                format!("playlists/{}/images", query.text),
                "",
                Some(RemoteEntityKind::Playlist),
                true,
            ),
            PlaylistItems => (format!("playlists/{}/items", query.text), "", None, false),
            AlbumTracks => (format!("albums/{}/tracks", query.text), "", None, false),
            ArtistAlbums | HomeArtistAlbums => (
                format!("artists/{}/albums", query.text),
                "",
                Some(RemoteEntityKind::Album),
                false,
            ),
            ArtistTopTracks => {
                params.clear();
                (
                    format!("artists/{}/top-tracks", query.text),
                    "",
                    None,
                    false,
                )
            }
            ArtistSelectedTracks => {
                let artist = self
                    .call(Method::GET, &format!("artists/{}", query.text), &[], None)?
                    .unwrap_or(Value::Null);
                let name: String = artist["name"]
                    .as_str()
                    .filter(|_| artist["id"].as_str() == Some(query.text.as_str()))
                    .ok_or_else(ApiFailure::network)?
                    .chars()
                    .filter(|c| !c.is_control() && !matches!(*c, '"' | '\\'))
                    .take(128)
                    .collect();
                if name.trim().is_empty() {
                    return Err(ApiFailure::network());
                }
                params.push(("q", format!("artist:\"{}\"", name.trim())));
                params.push(("type", "track".into()));
                ("search".into(), "tracks", None, false)
            }
            ShowEpisodes => (format!("shows/{}/episodes", query.text), "", None, false),
            AudiobookChapters => (
                format!("audiobooks/{}/chapters", query.text),
                "",
                None,
                false,
            ),
            FollowedArtists => {
                params.retain(|(key, _)| *key != "offset");
                params.push(("type", "artist".into()));
                if !query.text.is_empty() {
                    params.push(("after", query.text.clone()));
                }
                (
                    "me/following".into(),
                    "artists",
                    Some(RemoteEntityKind::Artist),
                    false,
                )
            }
            Recent | HomeRecent => {
                params.retain(|(key, _)| *key != "offset");
                if !query.text.is_empty() {
                    params.push(("before", query.text.clone()));
                }
                ("me/player/recently-played".into(), "", None, false)
            }
            TopTracks | TopArtists | HomeTopTracks | HomeTopArtists => {
                params.push((
                    "time_range",
                    if query.text.is_empty() {
                        "medium_term".into()
                    } else {
                        query.text.clone()
                    },
                ));
                if matches!(query.kind, TopTracks | HomeTopTracks) {
                    ("me/top/tracks".into(), "", None, false)
                } else {
                    (
                        "me/top/artists".into(),
                        "",
                        Some(RemoteEntityKind::Artist),
                        false,
                    )
                }
            }
            Album => (
                format!("albums/{}", query.text),
                "",
                Some(RemoteEntityKind::Album),
                true,
            ),
            Artist => (
                format!("artists/{}", query.text),
                "",
                Some(RemoteEntityKind::Artist),
                true,
            ),
            Show => (
                format!("shows/{}", query.text),
                "",
                Some(RemoteEntityKind::Show),
                true,
            ),
            Audiobook => (
                format!("audiobooks/{}", query.text),
                "",
                Some(RemoteEntityKind::Audiobook),
                true,
            ),
            Track => (format!("tracks/{}", query.text), "", None, true),
            Episode => (format!("episodes/{}", query.text), "", None, true),
            Chapter => (format!("chapters/{}", query.text), "", None, true),
            Profile => ("me".into(), "", Some(RemoteEntityKind::Profile), true),
        };
        if singleton {
            params.clear();
        }
        if matches!(query.kind, PlaylistInfo) {
            params.push((
                "fields",
                "id,name,description,public,images,owner(id,display_name),collaborative".into(),
            ));
        }
        if matches!(query.kind, PlaylistItems) {
            params.push(("additional_types", "track,episode".into()));
        }
        let result = self.call(Method::GET, &path, &params, None);
        if matches!(query.kind, Track) {
            match &result {
                Ok(detail) => {
                    self.remember_track_album(&query.text, detail.as_ref().unwrap_or(&Value::Null))
                }
                Err(failure) if failure.status == 404 => {
                    self.remember_track_album(&query.text, &Value::Null)
                }
                Err(_) => {}
            }
        }
        let value = result
            .map_err(|failure| catalog_failure(failure, &query.kind))?
            .unwrap_or(Value::Null);
        let value = if matches!(query.kind, PlaylistImages) {
            json!({"id":query.text,"name":"Playlist cover","images":value})
        } else {
            value
        };
        let page = if wrapper.is_empty() {
            &value
        } else {
            &value[wrapper]
        };
        let rows = if singleton {
            vec![page.clone()]
        } else {
            page[if matches!(query.kind, ArtistTopTracks) {
                "tracks"
            } else {
                "items"
            }]
            .as_array()
            .ok_or_else(ApiFailure::network)?
            .clone()
        };
        let mut items = vec![];
        let mut item_positions = vec![];
        let mut collections = vec![];
        let mut spoken_progress = vec![];
        let mut recent_plays = vec![];
        let artist_tracks = matches!(query.kind, ArtistTopTracks | ArtistSelectedTracks);
        let mut seen_artist_tracks = std::collections::HashSet::new();
        let spoken_kind = match query.kind {
            ShowEpisodes | SavedEpisodes | SearchEpisodes | Episode | PlaylistItems => {
                Some("episode")
            }
            AudiobookChapters | Chapter => Some("chapter"),
            _ => None,
        };
        for (position, entry) in rows.iter().take(10).enumerate() {
            let row = if entry["id"].is_string() {
                entry
            } else {
                entry
                    .get("item")
                    .or_else(|| entry.get("track"))
                    .or_else(|| entry.get("album"))
                    .or_else(|| entry.get("show"))
                    .or_else(|| entry.get("episode"))
                    .unwrap_or(entry)
            };
            if let Some(kind) = collection_kind {
                if kind == RemoteEntityKind::Profile {
                    if let Some(id) = row["id"].as_str().filter(|id| {
                        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
                    }) {
                        collections.push(RemoteCollection {
                            account_id: row["account_id"]
                                .as_str()
                                .filter(|id| valid_stable_account_id(id))
                                .map(str::to_owned),
                            owner_id: None,
                            collaborative: None,
                            id: id.into(),
                            kind,
                            public: None,
                            title: row["display_name"]
                                .as_str()
                                .unwrap_or("Your Spotify account")
                                .chars()
                                .take(512)
                                .collect(),
                            subtitle: "Spotify account".into(),
                            description: String::new(),
                            artwork: mapping::image(&row["images"]),
                            url: if id.len() <= 256
                                && id
                                    .bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
                            {
                                format!("https://open.spotify.com/user/{id}")
                            } else {
                                "https://open.spotify.com/".into()
                            },
                        });
                    }
                } else if let Some(mut collection) = mapping::collection_as(row, kind) {
                    if matches!(query.kind, ArtistAlbums) {
                        let format = match row["album_type"].as_str() {
                            Some("album") => "Album",
                            Some("single") => "Single / EP",
                            Some("compilation") => "Compilation",
                            _ => "Release",
                        };
                        collection.subtitle = match row["release_date"]
                            .as_str()
                            .and_then(|date| date.get(..4))
                            .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
                        {
                            Some(year) => format!("{format} · {year}"),
                            None => format.into(),
                        };
                    }
                    collections.push(collection);
                }
            } else {
                let mut row = row.clone();
                if matches!(query.kind, AudiobookChapters | Chapter) {
                    let expected_id = singleton.then_some(query.text.as_str());
                    if !normalize_chapter(&mut row, expected_id) {
                        continue;
                    }
                } else if row["type"].is_null() {
                    row["type"] = json!(match query.kind {
                        ShowEpisodes | SavedEpisodes | SearchEpisodes | Episode => "episode",
                        _ => "track",
                    });
                }
                if artist_tracks
                    && (row["type"].as_str() != Some("track")
                        || !row["artists"].as_array().is_some_and(|artists| {
                            artists
                                .iter()
                                .any(|artist| artist["id"].as_str() == Some(query.text.as_str()))
                        }))
                {
                    continue;
                }
                if let Some(item) = mapping::track(&row) {
                    if artist_tracks
                        && (!item.capabilities.play || !seen_artist_tracks.insert(item.id.clone()))
                    {
                        continue;
                    }
                    if let Some(kind) = spoken_kind
                        && row["type"].as_str() == Some(kind)
                        && (!singleton || row["id"].as_str() == Some(query.text.as_str()))
                    {
                        record_spoken_progress(&mut spoken_progress, &row, &item);
                    }
                    if matches!(query.kind, Recent | HomeRecent)
                        && let Some(play) = recent_play(entry, items.len() as u32)
                    {
                        recent_plays.push(play);
                    }
                    items.push(item);
                    item_positions.push(query.offset + position as u32);
                }
            }
        }
        let skipped = if artist_tracks {
            0
        } else {
            rows.len().saturating_sub(items.len() + collections.len())
        };
        let next_cursor = if page["next"].as_str().is_some() {
            if matches!(query.kind, FollowedArtists) {
                page["cursors"]["after"].as_str().map(str::to_owned)
            } else if matches!(query.kind, Recent) {
                page["cursors"]["before"].as_str().map(str::to_owned)
            } else {
                None
            }
        } else {
            None
        };
        let source_offset = page["offset"].as_u64();
        if matches!(query.kind, ArtistAlbums) && source_offset != Some(u64::from(query.offset)) {
            return Err(ApiFailure::network());
        }
        let page = RemotePage {
            position_edits: snapshot.is_some(),
            related_collections: if matches!(query.kind, Chapter) && items.is_empty() {
                vec![]
            } else {
                mapping::related_collections(page, &query.kind)
            },
            detail: query.kind.is_detail(),
            item_positions,
            next_cursor,
            provider: "spotify".into(),
            query_id: query.query_id,
            offset: query.offset,
            total: page["total"]
                .as_u64()
                .unwrap_or(if next_cursor_present(page) {
                    query.offset as u64 + rows.len() as u64 + 1
                } else {
                    query.offset as u64 + rows.len() as u64
                }),
            items,
            collections,
            spoken_progress,
            recent_plays,
            issue: (skipped > 0).then(|| {
                PlayerError::new(
                    ErrorCode::TrackUnavailable,
                    format!("{skipped} unavailable items were omitted."),
                )
            }),
        };
        let context_kind = match query.kind {
            AlbumTracks => Some("album"),
            PlaylistItems => Some("playlist"),
            _ => None,
        };
        if let Some(kind) = context_kind
            && source_offset == Some(u64::from(query.offset))
        {
            self.remember_context_page(ContextPageGuard {
                context: format!("spotify:{kind}:{}", query.text),
                query_id: query.query_id,
                snapshot: snapshot.clone(),
                rows: page
                    .item_positions
                    .iter()
                    .copied()
                    .zip(&page.items)
                    .filter(|(_, item)| item.capabilities.play)
                    .map(|(position, item)| (position, item.id.clone()))
                    .collect(),
            });
        }
        if let Some(snapshot) = snapshot {
            self.playlist_page = Some(PlaylistPageGuard {
                id: query.text.clone(),
                query_id: query.query_id,
                generation: self.generation,
                snapshot,
                total: page.total,
                positions: page.item_positions.clone(),
            });
        }
        Ok(page)
    }
    fn mixed_search(&self, query: &RemoteQuery) -> Result<RemotePage, ApiFailure> {
        use RemoteEntityKind::*;
        let value = self
            .call(
                Method::GET,
                "search",
                &[
                    ("q", query.text.clone()),
                    (
                        "type",
                        "track,album,artist,playlist,show,episode,audiobook".into(),
                    ),
                    ("limit", "10".into()),
                    ("offset", query.offset.to_string()),
                ],
                None,
            )
            .map_err(|failure| catalog_failure(failure, &query.kind))?
            .unwrap_or(Value::Null);
        let mut found_group = false;
        let mut total = 0;
        let mut skipped = 0;
        let mut items = Vec::new();
        let mut collections = Vec::new();
        let mut spoken_progress = Vec::new();
        for (entity, wrapper, collection_kind) in [
            ("track", "tracks", None),
            ("album", "albums", Some(Album)),
            ("artist", "artists", Some(Artist)),
            ("playlist", "playlists", Some(Playlist)),
            ("show", "shows", Some(Show)),
            ("episode", "episodes", None),
            ("audiobook", "audiobooks", Some(Audiobook)),
        ] {
            let group = &value[wrapper];
            let Some(rows) = group["items"].as_array() else {
                continue;
            };
            found_group = true;
            let fallback_total = u64::from(query.offset)
                + rows.len().min(10) as u64
                + u64::from(next_cursor_present(group));
            total = total.max(group["total"].as_u64().unwrap_or(fallback_total));
            for row in rows.iter().take(10) {
                if !row.is_object()
                    || (!row["type"].is_null() && row["type"].as_str() != Some(entity))
                {
                    skipped += 1;
                    continue;
                }
                if let Some(kind) = collection_kind {
                    if let Some(collection) = mapping::collection_as(row, kind) {
                        collections.push(collection);
                    } else {
                        skipped += 1;
                    }
                } else {
                    let item = if row["type"].is_null() {
                        let mut row = row.clone();
                        row["type"] = json!(entity);
                        mapping::track(&row)
                    } else {
                        mapping::track(row)
                    };
                    if let Some(item) = item {
                        if entity == "episode" {
                            record_spoken_progress(&mut spoken_progress, row, &item);
                        }
                        items.push(item);
                    } else {
                        skipped += 1;
                    }
                }
            }
        }
        if !found_group {
            return Err(ApiFailure::network());
        }
        Ok(RemotePage {
            position_edits: false,
            related_collections: vec![],
            detail: false,
            item_positions: vec![],
            next_cursor: None,
            provider: "spotify".into(),
            query_id: query.query_id,
            offset: query.offset,
            // Offsets advance once for all groups. The largest group controls
            // pagination. Spotify accepts search offsets through 1000.
            total: total.min(1010),
            items,
            collections,
            spoken_progress,
            recent_plays: vec![],
            issue: (skipped > 0).then(|| {
                PlayerError::new(
                    ErrorCode::TrackUnavailable,
                    format!("{skipped} unavailable items were omitted."),
                )
            }),
        })
    }
    pub fn edit(&mut self, edit: &RemoteEdit) -> Result<EditContent, ApiFailure> {
        if matches!(
            edit,
            RemoteEdit::PlaylistAdd { .. }
                | RemoteEdit::PlaylistRemove { .. }
                | RemoteEdit::PlaylistReplace { .. }
                | RemoteEdit::PlaylistDetails { .. }
                | RemoteEdit::PlaylistCover { .. }
        ) {
            self.playlist_page = None;
        }
        let (method, path, params, body) = match edit {
            RemoteEdit::CheckLibrary { uris } => {
                let value = self
                    .call(
                        Method::GET,
                        "me/library/contains",
                        &[("uris", uris.join(","))],
                        None,
                    )?
                    .unwrap_or(Value::Null);
                let rows = value
                    .as_array()
                    .filter(|rows| rows.len() == uris.len() && rows.iter().all(Value::is_boolean))
                    .ok_or_else(ApiFailure::network)?;
                return Ok(EditContent {
                    collection: None,
                    saved: uris
                        .iter()
                        .zip(rows)
                        .map(|(uri, saved)| RemoteSavedItem {
                            uri: uri.clone(),
                            saved: saved.as_bool().unwrap_or(false),
                        })
                        .collect(),
                });
            }
            RemoteEdit::PlaylistCover { id, jpeg_base64 } => {
                self.call_payload(
                    Method::PUT,
                    &format!("playlists/{id}/images"),
                    &[],
                    None,
                    Some(jpeg_base64),
                    false,
                )?;
                return Ok(EditContent::default());
            }

            RemoteEdit::Library { uris, save } => (
                if *save { Method::PUT } else { Method::DELETE },
                "me/library".into(),
                vec![("uris", uris.join(","))],
                None,
            ),
            RemoteEdit::CreatePlaylist {
                name,
                description,
                public,
                collaborative,
            } => {
                let mut body = json!({"name":name,"description":description,"public":public});
                if let Some(collaborative) = collaborative {
                    body["collaborative"] = json!(collaborative);
                }
                let value = self
                    .call_response(Method::POST, "me/playlists", &[], Some(body), true)?
                    .unwrap_or(Value::Null);
                return mapping::collection(&value)
                    .map(|collection| EditContent {
                        collection: Some(collection),
                        saved: vec![],
                    })
                    .ok_or_else(ApiFailure::network);
            }
            RemoteEdit::PlaylistDetails {
                id,
                name,
                description,
                public,
                collaborative,
            } => {
                let mut body = json!({});
                if let Some(name) = name {
                    body["name"] = json!(name);
                }
                if let Some(description) = description {
                    body["description"] = json!(description);
                }
                if let Some(public) = public {
                    body["public"] = json!(public);
                }
                if let Some(collaborative) = collaborative {
                    body["collaborative"] = json!(collaborative);
                }
                (Method::PUT, format!("playlists/{id}"), vec![], Some(body))
            }
            RemoteEdit::PlaylistAdd { id, uris } => (
                Method::POST,
                format!("playlists/{id}/items"),
                vec![],
                Some(json!({"uris":uris})),
            ),
            RemoteEdit::PlaylistRemove { id, uris } => (
                Method::DELETE,
                format!("playlists/{id}/items"),
                vec![],
                Some(
                    json!({"items":uris.iter().map(|uri| json!({"uri":uri})).collect::<Vec<_>>()}),
                ),
            ),
            RemoteEdit::PlaylistReplace { id, uris } => (
                Method::PUT,
                format!("playlists/{id}/items"),
                vec![],
                Some(json!({"uris":uris})),
            ),
            RemoteEdit::PlaylistMove {
                id,
                query_id,
                from,
                to,
            } => (
                Method::PUT,
                format!("playlists/{id}/items"),
                vec![],
                Some(
                    json!({"range_start":from,"insert_before":to,"range_length":1,"snapshot_id":self.move_snapshot(id, *query_id, *from, *to)?}),
                ),
            ),
        };
        self.call(method, &path, &params, body)
            .map_err(|mut failure| {
                if matches!(edit, RemoteEdit::PlaylistMove { .. })
                    && matches!(failure.status, 409 | 412)
                {
                    failure.error = playlist_changed().error;
                }
                failure
            })
            .map(|_| EditContent::default())
    }
}
fn next_cursor_present(page: &Value) -> bool {
    page["next"].as_str().is_some()
}
