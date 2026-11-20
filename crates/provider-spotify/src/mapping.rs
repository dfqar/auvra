use player_domain::*;
use serde_json::Value;
pub fn valid_id(value: &str) -> bool {
    value.len() == 22 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}
pub fn track_uri(item: &MediaItem) -> Result<String, PlayerError> {
    item.validate()?;
    let uri = item.id.as_str();
    let valid = ["track", "episode", "chapter"].iter().any(|kind| {
        uri.strip_prefix(&format!("spotify:{kind}:"))
            .is_some_and(valid_id)
    });
    if !valid {
        return Err(PlayerError::invalid("Choose a playable Spotify item"));
    }
    if !item.capabilities.play {
        return Err(PlayerError::new(
            ErrorCode::TrackUnavailable,
            "This track is not available for playback",
        ));
    }
    // Spotify's Chapter object has a chapter identity but an episode playback URI.
    Ok(uri
        .strip_prefix("spotify:chapter:")
        .map_or_else(|| uri.to_owned(), |id| format!("spotify:episode:{id}")))
}
pub(super) fn context_start_offset(
    context: &str,
    device: &str,
    item: Option<&MediaItem>,
) -> Result<Option<String>, PlayerError> {
    let valid_context = context
        .strip_prefix("spotify:")
        .and_then(|value| value.split_once(':'))
        .is_some_and(|(kind, id)| {
            valid_id(id)
                && (matches!(kind, "album" | "playlist") || kind == "artist" && item.is_none())
        });
    if !valid_context || !super::valid_device_id(device) {
        return Err(PlayerError::invalid("Invalid playback context or device"));
    }
    let Some(item) = item else { return Ok(None) };
    let uri = item.id.as_str();
    if !uri.starts_with("spotify:track:")
        || !matches!(&item.location, Some(MediaLocation::Remote { uri: location, .. }) if location == uri)
    {
        return Err(PlayerError::invalid(
            "Choose a Spotify song from this collection",
        ));
    }
    track_uri(item).map(Some)
}
pub(super) fn context_start_position(
    position: Option<u32>,
    query_id: Option<u64>,
    item: Option<&MediaItem>,
) -> Result<Option<(u32, u64)>, PlayerError> {
    match (position, query_id, item) {
        (None, None, _) => Ok(None),
        (Some(position), Some(query_id), Some(_)) if position < 100_000 && query_id > 0 => {
            Ok(Some((position, query_id)))
        }
        _ => Err(PlayerError::invalid(
            "Invalid playback row or catalog query",
        )),
    }
}
pub(super) fn spoken_start_uri(item: &MediaItem, position_ms: u64) -> Result<String, PlayerError> {
    let id = item.id.as_str();
    if !["spotify:episode:", "spotify:chapter:"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
        || !matches!(&item.location, Some(MediaLocation::Remote { uri, .. }) if uri == id)
        || position_ms == 0
        || position_ms >= item.duration_ms
    {
        return Err(PlayerError::invalid(
            "Invalid spoken item or playback position",
        ));
    }
    track_uri(item)
}
pub(super) fn same_playback_id(a: &MediaId, b: &MediaId) -> bool {
    a == b
        || [(a, b), (b, a)].into_iter().any(|(chapter, episode)| {
            chapter
                .as_str()
                .strip_prefix("spotify:chapter:")
                .filter(|id| valid_id(id))
                .is_some_and(|id| episode.as_str().strip_prefix("spotify:episode:") == Some(id))
        })
}
pub fn device(v: &Value) -> Option<PlaybackDevice> {
    let id = v["id"].as_str()?;
    if id.len() > 256 {
        return None;
    }
    Some(PlaybackDevice {
        id: id.into(),
        name: v["name"]
            .as_str()
            .unwrap_or("Spotify device")
            .chars()
            .take(256)
            .collect(),
        device_type: v["type"]
            .as_str()
            .unwrap_or("Unknown")
            .chars()
            .take(64)
            .collect(),
        volume: v["volume_percent"]
            .as_u64()
            .map(|n| n.min(100) as f32 / 100.0),
        is_active: v["is_active"].as_bool().unwrap_or(false),
        is_restricted: v["is_restricted"].as_bool().unwrap_or(false),
    })
}
pub fn queue_uri(item: &MediaItem) -> Result<String, PlayerError> {
    let uri = track_uri(item)?;
    if item.id.as_str().starts_with("spotify:chapter:") {
        return Err(PlayerError::invalid(
            "Only songs and podcast episodes can be added to this queue",
        ));
    }
    Ok(uri)
}
pub(super) fn queue_anchor(item: &Value) -> Option<MediaId> {
    let kind = item["type"].as_str()?;
    let id = item["id"].as_str().filter(|id| valid_id(id))?;
    if !matches!(kind, "track" | "episode") || item["is_local"].as_bool() == Some(true) {
        return None;
    }
    let uri = format!("spotify:{kind}:{id}");
    if item
        .get("uri")
        .is_some_and(|value| value.as_str() != Some(uri.as_str()))
    {
        return None;
    }
    MediaId::parse(uri).ok()
}
pub(crate) fn valid_image_url(value: &str) -> bool {
    value.len() <= 4096
        && [
            "https://i.scdn.co/",
            "https://mosaic.scdn.co/",
            "https://image-cdn-fa.spotifycdn.com/image/",
            "https://image-cdn-ak.spotifycdn.com/image/",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}
pub(crate) fn image(v: &Value) -> Option<String> {
    v.as_array()?.iter().find_map(|i| {
        i["url"]
            .as_str()
            .filter(|s| valid_image_url(s))
            .map(str::to_owned)
    })
}
pub fn track(v: &Value) -> Option<MediaItem> {
    match v["type"].as_str().unwrap_or("track") {
        "episode" | "chapter" => return spoken(v),
        "track" => {}
        _ => return None,
    }
    let id = v["id"].as_str().filter(|id| valid_id(id))?;
    let playable = v["is_playable"].as_bool().unwrap_or(true)
        && v["restrictions"].as_object().is_none_or(|r| r.is_empty())
        && v["is_local"] != true
        && v["type"].as_str().unwrap_or("track") == "track";
    let mut capabilities = Capabilities::REMOTE;
    capabilities.play = playable;
    let item = MediaItem {
        id: MediaId::parse(format!("spotify:track:{id}")).ok()?,
        title: v["name"].as_str()?.chars().take(512).collect(),
        artists: v["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .filter_map(|a| a["name"].as_str().map(|s| s.chars().take(256).collect()))
            .collect(),
        album: v["album"]["name"]
            .as_str()
            .map(|s| s.chars().take(512).collect()),
        duration_ms: v["duration_ms"].as_u64().unwrap_or(0).min(86_400_000),
        artwork: image(&v["album"]["images"]),
        location: Some(MediaLocation::Remote {
            uri: format!("spotify:track:{id}"),
            url: format!("https://open.spotify.com/track/{id}"),
            service: "Spotify".into(),
        }),
        capabilities,
    };
    item.validate().ok()?;
    Some(item)
}
pub fn collection(v: &Value) -> Option<RemoteCollection> {
    collection_as(v, RemoteEntityKind::Playlist)
}
fn creator_names(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value["name"].as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .take(16)
        .map(|name| name.chars().take(256).collect())
        .collect()
}
pub fn collection_as(v: &Value, kind: RemoteEntityKind) -> Option<RemoteCollection> {
    let id = v["id"].as_str().filter(|id| valid_id(id))?;
    let entity = match kind {
        RemoteEntityKind::Playlist => "playlist",
        RemoteEntityKind::Album => "album",
        RemoteEntityKind::Artist => "artist",
        RemoteEntityKind::Show => "show",
        RemoteEntityKind::Audiobook => "audiobook",
        RemoteEntityKind::Profile => "user",
    };
    Some(RemoteCollection {
        kind,
        account_id: None,
        owner_id: if kind == RemoteEntityKind::Playlist {
            v["owner"]["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .map(str::to_owned)
        } else {
            None
        },
        collaborative: if kind == RemoteEntityKind::Playlist {
            v["collaborative"].as_bool()
        } else {
            None
        },
        public: v["public"].as_bool(),
        subtitle: Some(
            creator_names(
                &v[if kind == RemoteEntityKind::Audiobook {
                    "authors"
                } else {
                    "artists"
                }],
            )
            .join(", "),
        )
        .filter(|s| !s.is_empty())
        .or_else(|| v["publisher"].as_str().map(str::to_owned))
        .or_else(|| v["owner"]["display_name"].as_str().map(str::to_owned))
        .unwrap_or_default()
        .chars()
        .take(512)
        .collect(),
        description: v["description"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(2048)
            .collect(),
        id: id.into(),
        title: v["name"].as_str()?.chars().take(512).collect(),
        url: format!("https://open.spotify.com/{entity}/{id}"),
        artwork: image(&v["images"]),
    })
}
/// Only singleton item details expose their real API parent/creator relationships.
pub fn related_collections(v: &Value, kind: &RemoteCatalogKind) -> Vec<RemoteCollection> {
    let mut result: Vec<RemoteCollection> = vec![];
    let mut add = |value: &Value, entity| {
        if let Some(collection) = collection_as(value, entity)
            && !result
                .iter()
                .any(|old| old.kind == collection.kind && old.id == collection.id)
        {
            result.push(collection);
        }
    };
    match kind {
        RemoteCatalogKind::Track => {
            add(&v["album"], RemoteEntityKind::Album);
            for artist in v["artists"].as_array().into_iter().flatten().take(16) {
                add(artist, RemoteEntityKind::Artist);
            }
        }
        RemoteCatalogKind::Episode => add(&v["show"], RemoteEntityKind::Show),
        RemoteCatalogKind::Chapter => add(&v["audiobook"], RemoteEntityKind::Audiobook),
        _ => {}
    }
    result
}
pub fn playback(v: &Value) -> RemotePlayback {
    let device = device(&v["device"]);
    let usable = device.as_ref().is_some_and(|d| !d.is_restricted);
    let actions = &v["actions"];
    let disallows = &actions["disallows"];
    let blocked = |name: &str| {
        !usable || actions[name].as_bool() == Some(true) || disallows[name].as_bool() == Some(true)
    };
    let current = track(&v["item"]);
    let mut capabilities = Capabilities::REMOTE;
    capabilities.play =
        current.as_ref().is_some_and(|i| i.capabilities.play) && !blocked("resuming");
    capabilities.pause = current.is_some() && !blocked("pausing");
    capabilities.seek = current.is_some() && !blocked("seeking");
    capabilities.volume = usable && v["device"]["supports_volume"].as_bool().unwrap_or(false);
    let queue_add = usable && current.is_some() && device.as_ref().is_some_and(|d| d.is_active);
    RemotePlayback {
        enhancement: None,
        position_rate: 1.0,
        context_uri: v["context"]["uri"]
            .as_str()
            .filter(|uri| valid_uri(uri))
            .map(str::to_owned),
        state: if v["is_playing"].as_bool().unwrap_or(false) {
            PlaybackState::Playing
        } else {
            PlaybackState::Paused
        },
        position_ms: v["progress_ms"]
            .as_u64()
            .unwrap_or(0)
            .min(current.as_ref().map_or(0, |i| i.duration_ms)),
        volume: device.as_ref().and_then(|d| d.volume).unwrap_or(0.0),
        current,
        upcoming: vec![],
        device_queue: None,
        shuffle: v["shuffle_state"].as_bool().unwrap_or(false),
        repeat: match v["repeat_state"].as_str() {
            Some("track") => RepeatMode::One,
            Some("context") => RepeatMode::All,
            _ => RepeatMode::Off,
        },
        capabilities,
        transport: TransportControls {
            queue_add,
            seek_forward_ms: None,
            seek_backward_ms: None,
            stop: false,
            next: !blocked("skipping_next"),
            previous: !blocked("skipping_prev"),
            shuffle: !blocked("toggling_shuffle"),
            repeat: !blocked("toggling_repeat_context") && !blocked("toggling_repeat_track"),
            queue_edit: false,
            retry: false,
        },
        device,
    }
}

pub fn valid_uri(uri: &str) -> bool {
    [
        "track",
        "episode",
        "chapter",
        "album",
        "artist",
        "playlist",
        "show",
        "audiobook",
    ]
    .iter()
    .any(|kind| {
        uri.strip_prefix(&format!("spotify:{kind}:"))
            .is_some_and(valid_id)
    })
}
pub fn valid_library_uri(uri: &str) -> bool {
    if let Some(id) = uri.strip_prefix("spotify:user:") {
        return (1..=128).contains(&id.len())
            && !matches!(id, "." | "..")
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b));
    }
    valid_uri(uri)
}
fn spoken(v: &Value) -> Option<MediaItem> {
    let kind = v["type"].as_str()?;
    let id = v["id"].as_str().filter(|id| valid_id(id))?;
    let mut capabilities = Capabilities::REMOTE;
    capabilities.library_save = kind != "chapter";
    capabilities.play = v["is_playable"].as_bool().unwrap_or(true)
        && v["restrictions"].as_object().is_none_or(|r| r.is_empty());
    let parent = if kind == "episode" {
        &v["show"]
    } else {
        &v["audiobook"]
    };
    let mut creators = if kind == "chapter" {
        creator_names(&parent["authors"])
    } else {
        vec![]
    };
    if creators.is_empty()
        && let Some(name) = parent["publisher"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                (kind == "episode")
                    .then(|| parent["name"].as_str())
                    .flatten()
                    .filter(|name| !name.trim().is_empty())
            })
    {
        creators.push(name.trim().chars().take(256).collect());
    }
    let item = MediaItem {
        id: MediaId::parse(format!("spotify:{kind}:{id}")).ok()?,
        title: v["name"].as_str()?.chars().take(512).collect(),
        artists: creators,
        album: parent["name"]
            .as_str()
            .map(|s| s.chars().take(512).collect()),
        duration_ms: v["duration_ms"].as_u64().unwrap_or(0).min(86_400_000),
        artwork: image(&v["images"]).or_else(|| image(&parent["images"])),
        location: Some(MediaLocation::Remote {
            uri: format!("spotify:{kind}:{id}"),
            url: format!("https://open.spotify.com/{kind}/{id}"),
            service: "Spotify".into(),
        }),
        capabilities,
    };
    item.validate().ok()?;
    Some(item)
}
