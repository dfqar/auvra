use player_domain::*;
use player_queue::QueueSnapshot;
use serde::Serialize;
#[derive(Clone, Debug, Serialize)]
pub struct PlayerSnapshot {
    pub host_analysis_status: Option<player_domain::HostAnalysisStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_spatial_status: Option<HostSpatialStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_analysis_revoked_through: Option<u64>,
    pub enhancement: Option<PlaybackEnhancement>,
    pub restored_remote: Option<RestoredRemote>,
    pub revision: u64,
    pub state: PlaybackState,
    pub position_ms: u64,
    pub playback_rate: f64,
    pub volume: f32,
    pub sound: SoundState,
    pub output_statistics: Option<OutputStatistics>,
    pub active_provider: Option<String>,
    pub provider_connection: ProviderConnectionState,
    pub capabilities: Capabilities,
    pub transport: TransportControls,
    pub playback_session_active: bool,
    pub queue: QueueSnapshot,
    pub session: player_queue::SessionState,
    pub queue_issues: Vec<player_queue::QueueIssue>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SoundState {
    pub settings: SoundSettings,
    pub ready: bool,
    pub saving: bool,
    pub issue: Option<PlayerError>,
}
