use crate::PlayerSnapshot;
use player_domain::*;
use serde::Serialize;
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlayerEvent {
    AudioAnalysis {
        media_id: Option<MediaId>,
        frame: Option<Box<AnalysisFrame>>,
        modulation: Option<player_visuals::VisualModulation>,
    },
    RemoteState {
        state: Box<RemoteState>,
    },
    RemoteEdited {
        result: RemoteEditResult,
    },
    RemoteOutputCommand {
        provider: String,
        command: RemoteOutputCommand,
    },
    RemotePage {
        page: RemotePage,
    },
    LibraryState {
        state: LibraryState,
    },
    LibraryPage {
        page: LibraryPage,
    },
    MediaUpdated {
        item: MediaItem,
    },
    CommandResult {
        request_id: u64,
        error: Option<PlayerError>,
    },
    Snapshot {
        state: Box<PlayerSnapshot>,
    },
    Error {
        error: PlayerError,
    },
    Catalog {
        items: Vec<MediaItem>,
    },
}
