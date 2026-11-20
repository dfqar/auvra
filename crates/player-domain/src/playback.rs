use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
    Error,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Unavailable,
    Error,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybackDevice {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub device_type: String,
    pub volume: Option<f32>,
    pub is_active: bool,
    pub is_restricted: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderPlayback {
    pub state: PlaybackState,
    pub position_ms: u64,
    pub ended: bool,
}

/// Compact counters only. Never exposes PCM, device handles or callback work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputStatistics {
    pub callbacks: u64,
    pub media_frames: u64,
    pub nonzero_frames: u64,
    pub underrun_frames: u64,
    pub analysis_dropped_frames: u64,
}
