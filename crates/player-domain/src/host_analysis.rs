//! A native host may lease analysis of one authenticated output it owns.
//! Process verification and capture live in its platform adapter, never the domain.
use crate::{AnalysisFrame, MediaId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAnalysisLease {
    pub generation: u64,
    pub process_id: i32,
    pub provider: String,
    pub device_id: String,
    pub media_id: MediaId,
    #[serde(default)]
    pub spatial_stereo: bool,
    #[serde(default = "default_enabled")]
    pub visuals: bool,
    /// A paused spatial relay may retain its verified output with silent rendering.
    #[serde(default = "default_enabled")]
    pub playing: bool,
}
const fn default_enabled() -> bool {
    true
}
impl HostAnalysisLease {
    pub fn is_valid(&self) -> bool {
        self.generation > 0
            && self.process_id > 0
            && !self.provider.is_empty()
            && self.provider.len() <= 64
            && self.media_id.provider() == self.provider
            && !self.device_id.is_empty()
            && self.device_id.len() <= 256
            && (self.visuals || self.spatial_stereo)
            && (self.playing || self.spatial_stereo)
    }
    /// Surface visibility and ordinary pause/resume keep the same native stream.
    pub fn same_output(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.process_id == other.process_id
            && self.provider == other.provider
            && self.device_id == other.device_id
            && self.media_id == other.media_id
            && self.spatial_stereo == other.spatial_stereo
    }
}
/// All methods are nonblocking. Changing a lease clears old observations before
/// the capture worker can open its replacement. Only compact features leave it.
pub trait HostOutputAnalysis: Send {
    fn set_lease(&mut self, lease: Option<HostAnalysisLease>);
    /// Publish a lease and its pause evidence together. Adapters with a separate
    /// worker override this to make the whole update visible atomically.
    fn set_lease_with_pause_proofs(
        &mut self,
        lease: Option<HostAnalysisLease>,
        confirmed_pause: Option<u64>,
        renderer_only_pause: Option<u64>,
    ) {
        self.set_lease(lease);
        self.set_confirmed_spatial_pause(confirmed_pause);
        self.set_confirmed_renderer_only_pause(renderer_only_pause);
    }
    /// A retained spatial lease has a confirmed stationary pause on its owned output.
    fn set_confirmed_spatial_pause(&mut self, _generation: Option<u64>) {}
    /// A fresh paused clock followed real playback in the same owned occurrence.
    /// This can authorize synthetic renderer-only silence when capture never started.
    fn set_confirmed_renderer_only_pause(&mut self, _generation: Option<u64>) {}
    fn analysis(&self) -> Option<AnalysisFrame>;
    /// Revoke the previous capture before Next, optionally retaining a silent renderer.
    fn revoke_for_spatial_next(&mut self, _generation: u64) {
        self.set_lease(None);
    }
    /// Revoke the active capture during a witnessed automatic handoff.
    fn revoke_for_natural_spatial_next(&mut self, generation: u64) {
        self.revoke_for_spatial_next(generation);
    }
    /// Revoke pre-seek capture, optionally retaining a synthetic-silence renderer.
    fn revoke_for_spatial_seek(&mut self, _generation: u64) {
        self.set_lease(None);
    }
    /// A new position anchor in the same occurrence can retain only the renderer clock.
    fn revoke_for_spatial_reanchor(
        &mut self,
        _old_generation: u64,
        _replacement_generation: u64,
        _deadline: std::time::Instant,
    ) {
        self.set_lease(None);
    }
    fn cancel_spatial_next(&mut self) {}
    fn prepare_spatial_resume(&mut self, _generation: u64) -> bool {
        false
    }
    fn spatial_resume(&self, _generation: u64) -> SpatialResume {
        SpatialResume::Unavailable
    }
    fn cancel_spatial_resume(&mut self) {}
    /// Retire a paused renderer's retained native clock after a seek intent.
    /// Existing paused capture may remain; this never authorizes playback.
    fn invalidate_spatial_resume_after_seek(&mut self, _generation: u64) {}
    fn status(&self) -> HostAnalysisStatus {
        HostAnalysisStatus::Idle
    }
    fn spatial_status(&self) -> HostSpatialStatus {
        HostSpatialStatus::Idle
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialResume {
    #[default]
    Unavailable,
    Warming,
    Ready,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSpatialStatus {
    #[default]
    Idle,
    WaitingForRoute,
    Starting,
    Active,
    Paused,
    Direct,
    DirectProcess,
    DirectTap,
    DirectTapDevice,
    DirectTapFormat,
    DirectTapStart,
    DirectRenderer,
    DirectRendererInterrupted,
    DirectBuffer,
    DirectInputStalled,
    DirectOutputStalled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAnalysisStatus {
    #[default]
    Idle,
    Starting,
    WaitingForAudio,
    Running,
    UnsupportedSystem,
    UnverifiedOutput,
    ProcessUnavailable,
    TapUnavailable,
    DeviceUnavailable,
    UnsupportedFormat,
    StartUnavailable,
}
