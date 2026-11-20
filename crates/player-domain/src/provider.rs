use crate::*;
use std::time::Duration;

/// Called on a coordinator worker, never from a View, render tick or audio callback.
/// M1 is synchronous and in-memory. Network providers must use their own workers;
/// they report reconciled state rather than blocking this contract on HTTP.
pub trait PlaybackProvider: Send {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn connection(&self) -> ProviderConnectionState;
    fn validate(&self, item: &MediaItem) -> Result<(), PlayerError> {
        item.validate()
    }
    fn load(&mut self, item: &MediaItem) -> Result<ProviderPlayback, PlayerError>;
    /// Atomically choose the initial position and playback intent before any output.
    fn load_at(
        &mut self,
        item: &MediaItem,
        position_ms: u64,
        autoplay: bool,
    ) -> Result<ProviderPlayback, PlayerError> {
        if position_ms == 0 && autoplay {
            self.load(item)
        } else {
            Err(PlayerError::new(
                ErrorCode::UnsupportedCapability,
                "This source cannot prepare paused playback or resume a saved position",
            ))
        }
    }
    fn pause(&mut self) -> Result<ProviderPlayback, PlayerError>;
    fn resume(&mut self) -> Result<ProviderPlayback, PlayerError>;
    fn stop(&mut self) -> Result<(), PlayerError>;
    fn seek(&mut self, position_ms: u64) -> Result<ProviderPlayback, PlayerError>;
    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError>;
    fn set_equalizer(&mut self, _settings: EqualizerSettings) -> Result<(), PlayerError> {
        Err(PlayerError::new(
            ErrorCode::UnsupportedCapability,
            "This source has no local equalizer",
        ))
    }
    fn tick(&mut self, elapsed: Duration) -> Result<Option<ProviderPlayback>, PlayerError>;
    fn output_statistics(&self) -> Option<OutputStatistics> {
        None
    }
    /// Latest compact observation from presented PCM; unavailable sources return None.
    fn analysis(&self) -> Option<AnalysisFrame> {
        None
    }
    /// Metadata learned by an IO/decode worker; never probes a file on the coordinator thread.
    fn take_media_update(&mut self) -> Option<MediaItem> {
        None
    }
}

/// Catalog and playback can be registered independently, including a read-only catalog.
pub trait CatalogProvider: Send {
    fn id(&self) -> &str;
    fn search(&self, query: &str) -> Result<Vec<MediaItem>, PlayerError>;
    fn get_track(&self, id: &MediaId) -> Result<MediaItem, PlayerError>;
    /// Resolve an authorized path lexically. IO/metadata decoding belongs to a worker.
    fn resolve_file(&self, _path: &str) -> Result<MediaItem, PlayerError> {
        Err(PlayerError::new(
            ErrorCode::UnsupportedCapability,
            "This catalog cannot open files",
        ))
    }
}
