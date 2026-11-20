//! Persistent local library. All filesystem/SQLite work belongs to LibraryWorker.
mod database;
mod metadata;
mod scanner;
mod worker;
pub use database::Database;
use player_domain::{ErrorCode, PlayerError};
pub use worker::LibraryWorker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFingerprint {
    pub size_bytes: u64,
    pub modified_unix_ns: u64,
}
pub fn is_supported_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "flac" | "mp3" | "m4a" | "aac" | "alac" | "wav" | "aif" | "aiff" | "ogg"
    )
}
pub(crate) fn error(e: impl std::fmt::Display) -> PlayerError {
    PlayerError::new(ErrorCode::LibraryError, format!("Library: {e}"))
}
#[cfg(test)]
mod tests;
