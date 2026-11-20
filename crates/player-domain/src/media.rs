use crate::{Capabilities, PlayerError};
use serde::{Deserialize, Serialize};

/// provider:kind:opaque_id; the provider namespace is never inferred from artwork or a URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MediaId(String);
impl MediaId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PlayerError> {
        let value = value.into();
        let parts: Vec<_> = value.splitn(3, ':').collect();
        if value.len() > 512
            || parts.len() != 3
            || parts.iter().any(|p| p.is_empty())
            || parts.iter().take(2).any(|p| {
                !p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            })
            || value.chars().any(char::is_control)
        {
            return Err(PlayerError::invalid("Media ID must be provider:kind:id"));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn provider(&self) -> &str {
        self.0.split(':').next().unwrap_or_default()
    }
}
impl TryFrom<String> for MediaId {
    type Error = PlayerError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<MediaId> for String {
    fn from(value: MediaId) -> Self {
        value.0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaItem {
    pub id: MediaId,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub artwork: Option<String>,
    pub location: Option<MediaLocation>,
    pub capabilities: Capabilities,
}
impl MediaItem {
    pub fn validate(&self) -> Result<(), PlayerError> {
        let text_bytes = self.location.as_ref().map_or(0, |location| match location {
            MediaLocation::File { path } => path.len(),
            MediaLocation::Remote { uri, url, service } => uri.len() + url.len() + service.len(),
        }) + self.title.len()
            + self.artists.iter().map(String::len).sum::<usize>()
            + self.album.as_ref().map_or(0, String::len)
            + self.artwork.as_ref().map_or(0, String::len);
        if self.title.trim().is_empty() || self.artists.len() > 16 || text_bytes > 4096 {
            return Err(PlayerError::invalid("Track metadata is empty or too large"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum PlaybackContext {
    Album(MediaId),
    Playlist(MediaId),
    Search(String),
    LocalFolder(String),
    UserQueue,
}

/// An authorized locator, not an open file handle. Hosts own platform access grants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MediaLocation {
    File {
        path: String,
    },
    Remote {
        uri: String,
        url: String,
        service: String,
    },
}
