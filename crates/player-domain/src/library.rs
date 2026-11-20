//! IO-free library commands and projections. Hosts own access grants; workers own storage.
use crate::{MediaItem, PlayerError};
use serde::{Deserialize, Serialize};

pub const MAX_LIBRARY_ROOTS: usize = 64;
pub const MAX_LIBRARY_PAGE: u32 = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedRoot {
    pub id: String,
    pub name: String,
    /// None means the host could not restore access. A stored path is never an access grant.
    pub path: Option<String>,
}
impl AuthorizedRoot {
    pub fn validate(&self) -> Result<(), PlayerError> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || self.name.is_empty()
            || self.name.len() > 1024
            || self.path.as_ref().is_some_and(|p| {
                p.len() > 3500 || p.contains('\0') || !std::path::Path::new(p).is_absolute()
            })
        {
            return Err(PlayerError::invalid("Invalid library folder grant"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryMode {
    #[default]
    Tracks,
    Albums,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryQuery {
    pub query_id: u64,
    pub mode: LibraryMode,
    pub text: String,
    pub album_id: Option<String>,
    pub root_id: Option<String>,
    pub offset: u32,
    pub limit: u32,
}
impl LibraryQuery {
    pub fn validate(&self) -> Result<(), PlayerError> {
        if self.text.len() > 1024
            || self.album_id.as_ref().is_some_and(|s| s.len() > 128)
            || self.root_id.as_ref().is_some_and(|s| s.len() > 64)
            || self.limit == 0
            || self.limit > MAX_LIBRARY_PAGE
        {
            return Err(PlayerError::invalid("Invalid library query"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryTrack {
    pub item: MediaItem,
    pub album_id: String,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub available: bool,
    pub issue: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u32>,
    pub artwork: Option<String>,
    pub track_count: u64,
    pub available_count: u64,
    pub duration_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryRoot {
    pub id: String,
    pub name: String,
    pub path: String,
    pub authorized: bool,
    pub status: String,
    pub issue: Option<String>,
    pub track_count: u64,
    pub last_scan_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanProgress {
    pub root_id: String,
    pub visited: u64,
    pub imported: u64,
    pub unchanged: u64,
    pub failed: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LibraryState {
    pub revision: u64,
    pub ready: bool,
    pub roots: Vec<LibraryRoot>,
    pub total_tracks: u64,
    pub total_albums: u64,
    pub scan: Option<ScanProgress>,
    pub last_scan: Option<ScanProgress>,
    pub issue: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryPage {
    pub query_id: u64,
    pub revision: u64,
    pub offset: u32,
    pub total: u64,
    pub tracks: Vec<LibraryTrack>,
    pub albums: Vec<LibraryAlbum>,
}
/// Internal worker operations. No SQL or filesystem access is permitted in submit/poll.
pub enum LibraryAction {
    Initialize {
        directory: String,
        roots: Vec<AuthorizedRoot>,
    },
    AddRoot(AuthorizedRoot),
    RemoveRoot(String),
    Scan(Option<String>),
    CancelScan,
    Query(LibraryQuery),
    PreparePlayback {
        token: u64,
        query: LibraryQuery,
        track_id: String,
    },
}
pub enum LibraryUpdate {
    State(LibraryState),
    Page(LibraryPage),
    Playback {
        token: u64,
        items: Vec<MediaItem>,
        start: usize,
        album_id: Option<String>,
    },
    Error(PlayerError),
}
pub trait LibraryProvider: Send {
    fn submit(&mut self, action: LibraryAction) -> Result<(), PlayerError>;
    fn poll(&mut self) -> Option<LibraryUpdate>;
}
