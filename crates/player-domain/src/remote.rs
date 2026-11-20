//! Async remote control contracts. No HTTP, credentials persistence or platform APIs.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportControls {
    pub stop: bool,
    pub next: bool,
    pub previous: bool,
    pub shuffle: bool,
    pub repeat: bool,
    pub queue_edit: bool,
    #[serde(default)]
    pub queue_add: bool,
    #[serde(default)]
    pub seek_forward_ms: Option<u64>,
    #[serde(default)]
    pub seek_backward_ms: Option<u64>,
    #[serde(default)]
    pub retry: bool,
}
impl TransportControls {
    pub const LOCAL: Self = Self {
        stop: true,
        next: true,
        previous: true,
        shuffle: true,
        repeat: true,
        queue_edit: true,
        queue_add: true,
        seek_forward_ms: None,
        seek_backward_ms: None,
        retry: true,
    };
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteControl {
    Play,
    Pause,
    PauseOnDevice {
        device_id: String,
    },
    Next,
    Previous,
    Stop,
    Seek {
        position_ms: u64,
    },
    Volume {
        volume: f32,
    },
    Shuffle {
        enabled: bool,
    },
    Repeat {
        mode: RepeatMode,
    },
    Transfer {
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<u64>,
    },
    Start {
        item: MediaItem,
    },
    /// Transfer and start as one admitted intent, explicitly targeting this device.
    StartOnDevice {
        item: MediaItem,
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<u64>,
    },
    /// Start a catalog episode or chapter at its saved listening position.
    StartSpokenOnDevice {
        item: MediaItem,
        device_id: String,
        position_ms: u64,
        operation_id: u64,
    },
    ResumeOnDevice {
        item: MediaItem,
        device_id: String,
        position_ms: u64,
        context_uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<u64>,
    },
    StartContext {
        uri: String,
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<u64>,
        /// An optional track to start inside this album or playlist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<MediaItem>,
        /// The exact source row, paired with the catalog query that supplied it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<u64>,
    },
    Enqueue {
        item: MediaItem,
    },
}
impl RemoteControl {
    pub fn operation_id(&self) -> Option<u64> {
        match self {
            Self::Transfer { operation_id, .. }
            | Self::StartOnDevice { operation_id, .. }
            | Self::ResumeOnDevice { operation_id, .. }
            | Self::StartContext { operation_id, .. } => *operation_id,
            Self::StartSpokenOnDevice { operation_id, .. } => Some(*operation_id),
            _ => None,
        }
    }
}
/// A selected remote recording, never a persisted remote queue or playback lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSelection {
    pub provider: String,
    pub account_id: String,
    /// Immutable account identifier when supported. Legacy selections contain only the profile ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_account_id: Option<String>,
    pub current: MediaItem,
    pub position_ms: u64,
    pub context_uri: Option<String>,
}
impl RemoteSelection {
    pub fn validate(&self) -> Result<(), PlayerError> {
        self.current.validate()?;
        if self.provider.len() > 64
            || self.current.id.provider() != self.provider
            || self.account_id.is_empty()
            || self.account_id.len() > 256
            || self.account_id.chars().any(char::is_control)
            || self
                .stable_account_id
                .as_deref()
                .is_some_and(|id| !valid_stable_account_id(id))
            || self.position_ms > self.current.duration_ms
            || self.current.capabilities.pcm_analysis
            || self.current.capabilities.system_media_session
            || self.current.capabilities.equalizer
            || !matches!(&self.current.location, Some(MediaLocation::Remote { uri, .. }) if uri == self.current.id.as_str())
            || self.context_uri.as_ref().is_some_and(|uri| {
                !MediaId::parse(uri).is_ok_and(|id| id.provider() == self.provider)
            })
        {
            return Err(PlayerError::invalid("Invalid saved remote selection"));
        }
        Ok(())
    }
}
pub fn valid_stable_account_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(|c| c.is_control() || c.is_whitespace())
}
#[derive(Clone, Debug, Serialize)]
pub struct RestoredRemote {
    pub context_uri: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCatalogKind {
    SearchAll,
    Search,
    SavedTracks,
    Playlists,
    PlaylistItems,
    PlaylistInfo,
    PlaylistImages,
    SearchAlbums,
    SearchArtists,
    SearchPlaylists,
    SearchShows,
    SearchEpisodes,
    SearchAudiobooks,
    SavedAlbums,
    SavedShows,
    SavedEpisodes,
    SavedAudiobooks,
    FollowedArtists,
    Recent,
    TopTracks,
    TopArtists,
    AlbumTracks,
    ArtistAlbums,
    ArtistTopTracks,
    ArtistSelectedTracks,
    ShowEpisodes,
    AudiobookChapters,
    Album,
    Artist,
    Show,
    Audiobook,
    Track,
    Episode,
    Chapter,
    Profile,
    HomeRecent,
    HomeTopTracks,
    HomeTopArtists,
    HomeArtistAlbums,
    HomePlaylists,
    HomeRadio,
}
impl RemoteCatalogKind {
    /// Fixed first pages used by Home rather than the active catalog query.
    pub fn is_home_shelf(&self) -> bool {
        matches!(
            self,
            Self::HomeRecent
                | Self::HomeTopTracks
                | Self::HomeTopArtists
                | Self::HomeArtistAlbums
                | Self::HomePlaylists
                | Self::HomeRadio
        )
    }

    /// Independent inspector/account reads must not replace the visible catalog page.
    pub fn is_detail(&self) -> bool {
        self.is_home_shelf()
            || matches!(
                self,
                Self::PlaylistInfo
                    | Self::PlaylistImages
                    | Self::Album
                    | Self::Artist
                    | Self::ArtistAlbums
                    | Self::ArtistTopTracks
                    | Self::ArtistSelectedTracks
                    | Self::Show
                    | Self::Audiobook
                    | Self::Track
                    | Self::Episode
                    | Self::Chapter
                    | Self::Profile
            )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteQuery {
    pub query_id: u64,
    pub kind: RemoteCatalogKind,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub offset: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAction {
    /// Only a short-lived bearer credential crosses this boundary. Never publish it.
    Credential {
        access_token: String,
    },
    Disconnect,
    Activate,
    /// Adopt an already observed output only after account-bound restore resolves.
    AdoptOutput,
    Refresh,
    Visibility {
        visible: bool,
    },
    /// A host-owned output's compact observation. Never audio or a provider command.
    OutputClock {
        clock: Option<RemoteOutputClock>,
    },
    OutputQueue {
        queue: RemoteOutputQueue,
    },
    OutputCommandResult {
        result: RemoteOutputCommandResult,
    },
    Query {
        query: RemoteQuery,
    },
    Control {
        control: RemoteControl,
    },
    Edit {
        request: RemoteEditRequest,
    },
}
impl std::fmt::Debug for RemoteAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Credential { .. } => f.write_str("Credential([redacted])"),
            Self::Disconnect => f.write_str("Disconnect"),
            Self::Activate => f.write_str("Activate"),
            Self::AdoptOutput => f.write_str("AdoptOutput"),
            Self::Refresh => f.write_str("Refresh"),
            Self::Visibility { visible } => f.debug_tuple("Visibility").field(visible).finish(),
            Self::OutputClock { clock } => f.debug_tuple("OutputClock").field(clock).finish(),
            Self::OutputQueue { queue } => f.debug_tuple("OutputQueue").field(queue).finish(),
            Self::OutputCommandResult { result } => {
                f.debug_tuple("OutputCommandResult").field(result).finish()
            }
            Self::Query { query } => f.debug_tuple("Query").field(query).finish(),
            Self::Control { control } => f.debug_tuple("Control").field(control).finish(),
            Self::Edit { request } => f.debug_tuple("Edit").field(request).finish(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputClock {
    pub generation: u64,
    pub sequence: u64,
    pub device_id: String,
    pub media_id: Option<MediaId>,
    pub state: PlaybackState,
    pub position_ms: u64,
    pub rate: f64,
    pub active: bool,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub options: Option<RemoteOutputOptions>,
    #[serde(default)]
    pub actions: Option<RemoteOutputActions>,
    #[serde(default)]
    pub metadata: Option<RemoteOutputMetadata>,
    /// Monotonic host time of this clock, present on hosts with a command pipe.
    #[serde(default)]
    pub control_uptime_ms: Option<u64>,
    #[serde(default)]
    pub queue_epoch: Option<u64>,
    /// The helper verified a new occurrence of the same URI for this owned command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_same_media_next: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputQueue {
    pub generation: u64,
    pub device_id: String,
    pub media_id: MediaId,
    pub epoch: u64,
    pub sequence: u64,
    #[serde(default)]
    pub autoplay_ready: bool,
    pub previous: Vec<RemoteOutputQueueEntry>,
    pub upcoming: Vec<RemoteOutputQueueEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputQueueEntry {
    pub media_id: MediaId,
    pub source: Option<QueueEntrySource>,
    pub metadata: Option<RemoteOutputMetadata>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueEntrySource {
    Context,
    Queue,
    Autoplay,
}

/// Additional read-only rows supplied by a playback device.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceQueueDetails {
    pub previous: Vec<MediaItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub previous_sources: Vec<Option<QueueEntrySource>>,
    pub upcoming_sources: Vec<Option<QueueEntrySource>>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteOutputControl {
    Play,
    StartUri {
        uri: String,
        operation_id: u64,
    },
    Pause,
    Seek {
        position_ms: u64,
    },
    Next,
    Previous,
    /// Cue the verified current occurrence to zero, then send native Previous once.
    PreviousFromStart,
    Volume {
        percent: u8,
    },
    Shuffle {
        enabled: bool,
    },
    Repeat {
        mode: RepeatMode,
    },
    Enqueue {
        uri: String,
    },
    /// Append a fallback at an observed context end. The owned output
    /// independently verifies the same empty queue and repeat state.
    AutoplayEnqueue {
        uri: String,
    },
}
impl RemoteOutputControl {
    pub fn operation_id(&self) -> Option<u64> {
        match self {
            Self::StartUri { operation_id, .. } => Some(*operation_id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteOutputCommand {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    /// The exact occurrence required for a scoped native command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_queue_epoch: Option<u64>,
    /// The occurrence observed at dispatch for correlating playback updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_queue_epoch: Option<u64>,
    /// The pinned preceding context item verified for a scoped native Previous.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_previous_media_id: Option<MediaId>,
    pub generation: u64,
    pub sequence: u64,
    pub device_id: String,
    pub media_id: MediaId,
    pub expires_at_uptime_ms: u64,
    pub control: RemoteOutputControl,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputCommandResult {
    pub id: u64,
    pub generation: u64,
    pub accepted: bool,
}
/// Compact metadata observed on a host-owned output. Its identity is the clock's
/// media ID. The provider validates fields and chooses when to use this fallback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputMetadata {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub artwork: Option<String>,
    pub context_uri: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackEnhancement {
    Off,
    Recommendations,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputOptions {
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub enhancement: Option<PlaybackEnhancement>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOutputActions {
    pub play: bool,
    pub pause: bool,
    pub seek: bool,
    pub next: bool,
    pub previous: bool,
    pub shuffle: bool,
    pub repeat: bool,
    pub queue_add: bool,
    pub seek_forward_ms: Option<u64>,
    pub seek_backward_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemotePlayback {
    pub enhancement: Option<PlaybackEnhancement>,
    pub context_uri: Option<String>,
    pub state: PlaybackState,
    pub position_ms: u64,
    pub position_rate: f64,
    pub volume: f32,
    pub current: Option<MediaItem>,
    pub upcoming: Vec<MediaItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_queue: Option<DeviceQueueDetails>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub capabilities: Capabilities,
    pub transport: TransportControls,
    pub device: Option<PlaybackDevice>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemoteState {
    /// Playback was observed on the authenticated owned output. Web API health
    /// still governs catalog access and HTTP commands.
    pub owned_output_observed: bool,
    pub owned_output_control: bool,
    /// Latest correlated HTTP dispatch result; it does not confirm playback.
    pub control_result: Option<RemoteControlResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_same_media_next_request_id: Option<u64>,
    /// Changes only when a fresh remote playback observation has been received.
    pub observation_id: u64,
    pub provider: String,
    pub connection: ProviderConnectionState,
    pub authentication_required: bool,
    pub issue: Option<PlayerError>,
    pub retry_after_seconds: Option<u64>,
    pub devices: Vec<PlaybackDevice>,
    pub playback: Option<RemotePlayback>,
}
impl RemoteState {
    pub fn transport_available(&self) -> bool {
        !self.authentication_required
            && (self.connection == ProviderConnectionState::Connected || self.owned_output_control)
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct RemoteControlResult {
    pub operation_id: u64,
    pub error: Option<PlayerError>,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEntityKind {
    #[default]
    Playlist,
    Album,
    Artist,
    Show,
    Audiobook,
    Profile,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemoteCollection {
    pub kind: RemoteEntityKind,
    /// Present only on an authenticated profile, distinct from its public user ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub owner_id: Option<String>,
    pub collaborative: Option<bool>,
    pub public: Option<bool>,
    pub subtitle: String,
    pub description: String,
    pub id: String,
    pub title: String,
    pub url: String,
    pub artwork: Option<String>,
}
/// Account-scoped progress carried by a catalog page, separate from the playback session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpokenProgress {
    pub media_id: MediaId,
    pub position_ms: u64,
    pub fully_played: bool,
}
/// Metadata for one emitted history row. A track can occur multiple times on a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecentPlay {
    pub item_index: u32,
    pub played_at: Option<String>,
    pub context_uri: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemotePage {
    /// Positional edits require a provider-owned version bound to this query.
    pub position_edits: bool,
    pub related_collections: Vec<RemoteCollection>,
    pub detail: bool,
    pub item_positions: Vec<u32>,
    pub next_cursor: Option<String>,
    pub provider: String,
    pub query_id: u64,
    pub offset: u32,
    pub total: u64,
    pub items: Vec<MediaItem>,
    pub collections: Vec<RemoteCollection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spoken_progress: Vec<SpokenProgress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_plays: Vec<RecentPlay>,
    pub issue: Option<PlayerError>,
}
#[derive(Clone, Debug)]
pub enum RemoteUpdate {
    Edited(RemoteEditResult),
    State(Box<RemoteState>),
    Page(RemotePage),
    OutputCommand(RemoteOutputCommand),
    OutputCommandFailed {
        control: RemoteOutputControl,
        error: PlayerError,
    },
}
/// Internal evidence for one authenticated output's capture ownership.
/// This grants no transport permissions and is never serialized across the ABI.
/// The identifier and original Playing anchor are immutable for this occurrence;
/// callers must also validate the current host lease and fresh matching clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedCaptureWitness {
    pub id: u64,
    pub generation: u64,
    pub device_id: String,
    pub media_id: MediaId,
    pub queue_epoch: u64,
    pub context_uri: String,
    pub playing_sequence: u64,
    pub playing_control_uptime_ms: u64,
}

pub trait RemoteProvider: Send {
    fn id(&self) -> &str;
    /// Nonblocking, provider-validated current capture ownership. A missing
    /// control binding remains missing; this cannot authorize native commands.
    fn owned_capture_witness(&self) -> Option<OwnedCaptureWitness> {
        None
    }
    /// Match a host output's media identity to the provider's catalog identity.
    fn output_media_matches(&self, observed: &MediaId, catalog: &MediaId) -> bool {
        observed == catalog
    }
    /// Must only enqueue bounded work; no network or blocking IO on the coordinator.
    fn submit(&mut self, action: RemoteAction) -> Result<(), PlayerError>;
    fn submit_next_with_request_id(&mut self, _request_id: u64) -> Result<(), PlayerError> {
        self.submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
    }
    /// May poll the in-memory owned queue immediately after publishing this request's receipt.
    fn owned_next_ready_for_request(&self, _request_id: u64) -> bool {
        false
    }
    /// Correlate the host request if Previous reaches the exact native cue path.
    fn submit_previous_with_request_id(&mut self, _request_id: u64) -> Result<(), PlayerError> {
        self.submit(RemoteAction::Control {
            control: RemoteControl::Previous,
        })
    }
    fn submit_seek_with_request_id(
        &mut self,
        position_ms: u64,
        _request_id: u64,
    ) -> Result<(), PlayerError> {
        self.submit(RemoteAction::Control {
            control: RemoteControl::Seek { position_ms },
        })
    }
    /// May poll the in-memory owned queue immediately after publishing this request's receipt.
    fn owned_seek_ready_for_request(&self, _request_id: u64) -> bool {
        false
    }
    fn submit_play_pause_with_request_id(
        &mut self,
        play: bool,
        _request_id: u64,
    ) -> Result<(), PlayerError> {
        self.submit(RemoteAction::Control {
            control: if play {
                RemoteControl::Play
            } else {
                RemoteControl::Pause
            },
        })
    }
    /// Keep an explicit pause destination while correlating native dispatch.
    fn submit_pause_on_device_with_request_id(
        &mut self,
        device_id: &str,
        _request_id: u64,
    ) -> Result<(), PlayerError> {
        self.submit(RemoteAction::Control {
            control: RemoteControl::PauseOnDevice {
                device_id: device_id.into(),
            },
        })
    }
    /// May poll the in-memory owned queue immediately after publishing this request's receipt.
    fn owned_play_pause_ready_for_request(&self, _request_id: u64) -> bool {
        false
    }
    /// A confirmed owned navigation handoff can use an immediate bounded provider poll.
    fn owned_handoff_ready(&self) -> bool {
        false
    }
    /// A Previous queued behind a pending Next must use the same owned output.
    fn submit_previous_after_next(&mut self, _device_id: &str) -> Result<(), PlayerError> {
        Err(PlayerError::new(
            ErrorCode::UnsupportedCapability,
            "The active source does not support this control",
        ))
    }
    fn poll(&mut self) -> Option<RemoteUpdate>;
    /// A locally dispatched start can still make sound while its device is
    /// reporting the previous paused selection.
    fn has_pending_output_start(&self) -> bool {
        false
    }
    /// Cancel the exact pending Resume only if the provider has not dispatched its write.
    fn cancel_resume_before_dispatch(&mut self, _operation_id: u64, _device_id: &str) -> bool {
        false
    }
    /// Admit one native Pause for the exact confirmed Resume. False means no
    /// native command was sent and the coordinator may use addressed HTTP.
    fn submit_owned_pause_after_resume(
        &mut self,
        _operation_id: u64,
        _device_id: &str,
        _request_id: u64,
    ) -> Result<bool, PlayerError> {
        Ok(false)
    }
}

/// Account mutations are explicit, bounded and performed on the catalog IO worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteEdit {
    CheckLibrary {
        uris: Vec<String>,
    },
    PlaylistCover {
        id: String,
        jpeg_base64: String,
    },
    Library {
        uris: Vec<String>,
        save: bool,
    },
    CreatePlaylist {
        name: String,
        description: String,
        public: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        collaborative: Option<bool>,
    },
    PlaylistDetails {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        public: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        collaborative: Option<bool>,
    },
    PlaylistAdd {
        id: String,
        uris: Vec<String>,
    },
    PlaylistRemove {
        id: String,
        uris: Vec<String>,
    },
    PlaylistMove {
        id: String,
        query_id: u64,
        from: u32,
        to: u32,
    },
    PlaylistReplace {
        id: String,
        uris: Vec<String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEditRequest {
    pub operation_id: u64,
    pub edit: RemoteEdit,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemoteEditResult {
    pub saved: Vec<RemoteSavedItem>,
    pub operation_id: u64,
    pub collection: Option<RemoteCollection>,
    pub error: Option<PlayerError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteSavedItem {
    pub uri: String,
    pub saved: bool,
}
