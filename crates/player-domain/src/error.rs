use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidCommand,
    CatalogChanged,
    ProviderUnavailable,
    AuthenticationRequired,
    NetworkError,
    TrackUnavailable,
    FileMissing,
    DecodeError,
    AudioDeviceLost,
    UnsupportedFormat,
    PermissionLost,
    RateLimited,
    UnsupportedCapability,
    LibraryError,
    DatabaseError,
    LibraryBusy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct PlayerError {
    pub code: ErrorCode,
    pub message: String,
}
impl PlayerError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidCommand, message)
    }
    pub fn unavailable(provider: &str) -> Self {
        Self::new(
            ErrorCode::ProviderUnavailable,
            format!("{provider} playback is not connected yet"),
        )
    }
}
