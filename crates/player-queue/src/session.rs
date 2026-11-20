//! IO-free persistence contract. A separate worker owns storage and access checks.
use crate::QueueEngine;
use player_domain::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportSession {
    pub version: u32,
    #[serde(default)]
    pub remote: Option<RemoteSelection>,
    pub queue: QueueEngine,
    pub position_ms: u64,
    pub volume: f32,
    pub stopped: bool,
    #[serde(default)]
    pub root_paths: Vec<SessionRoot>,
}
impl TransportSession {
    pub fn validate(&self) -> Result<(), PlayerError> {
        if ![1, 2].contains(&self.version)
            || !self.volume.is_finite()
            || !(0.0..=1.0).contains(&self.volume)
            || self.version == 1 && self.remote.is_some()
        {
            return Err(PlayerError::invalid(
                "Saved transport has an unsupported version or invalid volume",
            ));
        }
        if let Some(remote) = &self.remote {
            remote.validate()?;
            if remote.current.artwork.is_some() {
                return Err(PlayerError::invalid(
                    "Temporary remote artwork must not be persisted",
                ));
            }
        }
        if self.root_paths.len() > 256
            || self.root_paths.iter().any(|r| {
                r.id.is_empty()
                    || r.id.len() > 64
                    || r.path.len() > 3500
                    || r.path.contains('\0')
                    || !std::path::Path::new(&r.path).is_absolute()
            })
        {
            return Err(PlayerError::invalid(
                "Saved queue folder mappings are invalid",
            ));
        }
        self.queue.validate()
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAccess {
    pub roots: Vec<SessionRoot>,
    pub files: Vec<FileGrant>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRoot {
    pub id: String,
    pub path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileGrant {
    pub original_path: String,
    pub path: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct QueueIssue {
    pub id: MediaId,
    pub error: PlayerError,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionState {
    pub enabled: bool,
    pub ready: bool,
    pub restored: bool,
    pub saving: bool,
    pub issue: Option<PlayerError>,
}
pub enum SessionAction {
    SaveSound {
        serial: u64,
        settings: SoundSettings,
    },
    ReloadSound,
    Initialize {
        directory: String,
        access: SessionAccess,
    },
    Save {
        serial: u64,
        session: Box<TransportSession>,
    },
    SaveRemote {
        serial: u64,
        selection: Option<Box<RemoteSelection>>,
    },
    Validate {
        revision: u64,
        queue: Box<QueueEngine>,
        access: SessionAccess,
    },
}
pub enum SessionUpdate {
    SoundLoaded(Result<SoundSettings, PlayerError>),
    SoundSaved {
        serial: u64,
        result: Result<(), PlayerError>,
    },
    Loaded {
        session: Option<Box<TransportSession>>,
        issues: Vec<QueueIssue>,
    },
    Validated {
        revision: u64,
        queue: Box<QueueEngine>,
        issues: Vec<QueueIssue>,
    },
    Saved {
        serial: u64,
    },
    Error(PlayerError),
}
pub trait SessionStore: Send {
    fn submit(&mut self, action: SessionAction) -> Result<(), PlayerError>;
    fn poll(&mut self) -> Option<SessionUpdate>;
}
