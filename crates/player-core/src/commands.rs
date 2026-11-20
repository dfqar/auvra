use player_domain::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlayerCommand {
    /// Confirms an unexpected exit of the exact host-owned child. No provider IO.
    OwnedOutputInterrupted {
        provider: String,
        generation: u64,
        sequence: u64,
        device_id: String,
    },
    /// Cancels only inert crash-recovery provenance before explicit host restart.
    CancelOwnedOutputRecovery {
        provider: String,
    },
    HostAnalysis {
        lease: Option<HostAnalysisLease>,
    },
    /// Release one old media generation. Unlike full host revocation, this may
    /// preserve an independently proven, bounded natural successor preroll.
    RetireHostAnalysis {
        generation: u64,
    },
    HostAnalysisAfterSeek {
        lease: HostAnalysisLease,
        request_id: u64,
        sequence: u64,
    },
    HostAnalysisAfterClockChange {
        lease: HostAnalysisLease,
        retired_generation: u64,
        origin_sequence: u64,
        sequence: u64,
    },
    SetSound {
        settings: player_domain::SoundSettings,
    },
    RetrySoundStorage,
    RetryPlayback,
    Remote {
        provider: String,
        action: RemoteAction,
    },
    InitializeSession {
        directory: String,
        access: player_queue::SessionAccess,
    },
    RefreshSessionAccess {
        access: player_queue::SessionAccess,
    },
    JumpQueue {
        revision: u64,
        index: usize,
    },
    RemoveQueueItem {
        revision: u64,
        index: usize,
    },
    ClearQueue,
    InitializeLibrary {
        directory: String,
        roots: Vec<AuthorizedRoot>,
    },
    AddLibraryRoot {
        root: AuthorizedRoot,
    },
    RemoveLibraryRoot {
        id: String,
    },
    ScanLibrary {
        root_id: Option<String>,
    },
    CancelLibraryScan,
    QueryLibrary {
        query: LibraryQuery,
    },
    PlayLibrary {
        query: LibraryQuery,
        track_id: String,
    },
    OpenFile {
        path: String,
    },
    ReplaceQueue {
        items: Vec<MediaItem>,
        start: usize,
        context: PlaybackContext,
    },
    PlayNext {
        item: MediaItem,
    },
    Play,
    Pause,
    CancelRestoredResume {
        operation_id: u64,
        device_id: String,
    },
    PauseConfirmedRestoredResume {
        operation_id: u64,
        device_id: String,
    },
    Stop,
    Next,
    Previous,
    /// The host has already chosen the previous queue item at the time of input.
    SkipPrevious,
    Seek {
        position_ms: u64,
    },
    SetVolume {
        volume: f32,
    },
    SetShuffle {
        enabled: bool,
    },
    SetRepeat {
        mode: RepeatMode,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub request_id: u64,
    pub command: PlayerCommand,
}
