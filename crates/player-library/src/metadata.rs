use crate::{FileFingerprint, error};
use audio_engine::FileDecoder;
use player_domain::*;
use std::{fs, path::Path};

#[derive(Clone)]
pub(crate) struct ScannedTrack {
    pub item: MediaItem,
    pub album_id: String,
    pub album_title: String,
    pub album_artist: String,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub artwork_hash: Option<String>,
    pub issue: Option<String>,
}
pub(crate) fn fingerprint(metadata: &fs::Metadata) -> Result<FileFingerprint, PlayerError> {
    Ok(FileFingerprint {
        size_bytes: metadata.len(),
        modified_unix_ns: metadata
            .modified()
            .map_err(error)?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(error)?
            .as_nanos()
            .min(i64::MAX as u128) as u64,
    })
}
fn bounded(value: &str, bytes: usize) -> String {
    let end = (0..=value.len().min(bytes))
        .rev()
        .find(|&at| value.is_char_boundary(at))
        .unwrap_or(0);
    value[..end].trim().to_owned()
}
pub(crate) fn skeleton(path: &Path) -> Result<ScannedTrack, PlayerError> {
    let path = path
        .to_str()
        .filter(|s| s.len() <= 3500)
        .ok_or_else(|| error("File path is not valid UTF-8 or is too long"))?;
    let title = bounded(
        Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled track"),
        256,
    );
    let album_title = bounded(
        Path::new(path)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown album"),
        256,
    );
    let item = MediaItem {
        id: MediaId::parse(format!(
            "local:track:{}",
            blake3::hash(path.as_bytes()).to_hex()
        ))?,
        title,
        artists: vec![],
        album: Some(album_title.clone()),
        duration_ms: 0,
        artwork: None,
        location: Some(MediaLocation::File { path: path.into() }),
        capabilities: Capabilities::LOCAL,
    };
    Ok(ScannedTrack {
        item,
        album_id: String::new(),
        album_title,
        album_artist: "Unknown artist".into(),
        year: None,
        track_number: None,
        disc_number: None,
        artwork_hash: None,
        issue: None,
    })
}
pub(crate) fn inspect(
    path: &Path,
    cache: &Path,
) -> Result<(ScannedTrack, Option<String>), PlayerError> {
    let mut warning = None;
    let mut track = skeleton(path)?;
    let mut decoder = FileDecoder::open(path)?;
    // A small decode verifies the selected codec and catches an immediately corrupt payload.
    if decoder.read_frames(1)?.is_empty() {
        return Err(error("Empty audio track"));
    }
    let info = &decoder.info;
    track.item.title = info
        .title
        .as_ref()
        .map(|s| bounded(s, 256))
        .filter(|s| !s.is_empty())
        .unwrap_or(track.item.title);
    track.item.artists = info
        .artists
        .iter()
        .map(|s| bounded(s, 128))
        .filter(|s| !s.is_empty())
        .take(4)
        .collect();
    if let Some(album) = info
        .album
        .as_ref()
        .map(|s| bounded(s, 256))
        .filter(|s| !s.is_empty())
    {
        track.album_title = album;
    }
    track.album_artist = info
        .album_artist
        .as_ref()
        .map(|s| bounded(s, 256))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if track.item.artists.is_empty() {
                "Unknown artist".into()
            } else {
                bounded(&track.item.artists.join(", "), 256)
            }
        });
    track.item.album = Some(track.album_title.clone());
    track.item.duration_ms = info.duration_ms;
    track.year = info.year;
    track.track_number = info.track_number;
    track.disc_number = info.disc_number;
    if let Some(artwork) = decoder.embedded_artwork() {
        let hash = blake3::hash(&artwork).to_hex().to_string();
        let extension = if artwork.starts_with(b"\x89PNG") {
            "png"
        } else {
            "jpg"
        };
        let name = format!("{hash}.{extension}");
        let final_path = cache.join(&name);
        if !final_path.is_file() {
            let saved = (|| -> std::io::Result<()> {
                fs::create_dir_all(cache)?;
                let temporary = cache.join(format!(".{hash}.tmp"));
                fs::write(&temporary, &artwork)?;
                fs::rename(&temporary, &final_path)
            })();
            if let Err(e) = saved {
                warning = Some(format!("Artwork could not be cached: {e}"));
            }
        }
        track.artwork_hash = Some(name);
        track.item.artwork = Some(final_path.to_string_lossy().into_owned());
    }
    finish(&mut track, path)?;
    Ok((track, warning))
}
pub(crate) fn finish(track: &mut ScannedTrack, path: &Path) -> Result<(), PlayerError> {
    let group = if track.item.artists.is_empty() && track.album_artist == "Unknown artist" {
        format!(
            "{}\0{}",
            track.album_title.to_lowercase(),
            path.parent().unwrap_or(path).display()
        )
    } else {
        format!(
            "{}\0{}",
            track.album_title.to_lowercase(),
            track.album_artist.to_lowercase()
        )
    };
    track.album_id = format!("local:album:{}", blake3::hash(group.as_bytes()).to_hex());
    // Very long locators leave a smaller metadata budget in the existing control contract.
    if track.item.validate().is_err() {
        track.item.artists.clear();
        track.item.album = None;
        track.item.artwork = None;
    }
    track.item.validate()
}
