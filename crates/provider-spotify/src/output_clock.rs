//! Bounded, IO-free projection of the host's authenticated output clock.
use crate::RateLimit;
use player_domain::*;
use std::time::{Duration, Instant};

const EXPIRY: Duration = Duration::from_secs(6);
pub(super) const CONTROL_FRESHNESS: Duration = Duration::from_millis(900);
pub(super) const QUEUE_EXPIRY: Duration = Duration::from_secs(18);
const SELECTION_EXPIRY: Duration = Duration::from_secs(10);
const RESUME_POSITION_SLOP_MS: u64 = 300;
const RESUME_PAUSE_WINDOW: Duration = Duration::from_secs(2);

enum SelectionTarget {
    Track(MediaId),
    Context(String),
    ContextRow { uri: String, item: MediaId },
}

impl SelectionTarget {
    fn matches(&self, clock: &RemoteOutputClock) -> bool {
        let context = || {
            clock
                .metadata
                .as_ref()
                .and_then(|item| item.context_uri.as_ref())
        };
        match self {
            Self::Track(id) => clock.media_id.as_ref() == Some(id),
            Self::Context(uri) => context() == Some(uri),
            Self::ContextRow { uri, item } => {
                clock.media_id.as_ref() == Some(item) && context() == Some(uri)
            }
        }
    }
}

struct HttpSelection {
    operation: u64,
    generation: u64,
    device: String,
    target: SelectionTarget,
    prior_context_media: Option<MediaId>,
    sent_at: Instant,
    accepted: Option<(u64, Option<MediaId>)>,
    confirmed_media: Option<MediaId>,
    inactive_restart: Option<InactiveContextRestart>,
    capture_promoted: bool,
}

/// A dispatch veto for one addressed HTTP start, never playback authority.
/// Unlike the existing inactive path, an active unbound occurrence must remain
/// exact through a possible 404 fallback; a later takeover cannot revive it.
pub(super) struct ActiveContextStart {
    media: MediaId,
    context: Option<String>,
    epoch: u64,
    sequence: u64,
    uptime: u64,
    expires: Instant,
    revoked: bool,
}

impl ActiveContextStart {
    pub(super) fn permits_dispatch(&self) -> bool {
        !self.revoked && Instant::now() < self.expires
    }

    pub(super) fn revoke(&mut self) {
        self.revoked = true;
    }

    fn matches(&self, clock: &RemoteOutputClock) -> bool {
        self.permits_dispatch()
            && clock.media_id.as_ref() == Some(&self.media)
            && clock
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.context_uri == self.context)
            && clock.queue_epoch == Some(self.epoch)
            && clock.sequence >= self.sequence
            && clock
                .control_uptime_ms
                .is_some_and(|uptime| uptime >= self.uptime)
    }
}

pub(super) fn active_context_rows_conflict(devices: &[PlaybackDevice], device: &str) -> bool {
    devices.iter().filter(|known| known.id == device).count() > 1
        || devices.iter().any(|known| {
            known.id == device && known.is_restricted || known.id != device && known.is_active
        })
}

pub(super) fn active_context_state_conflicts(state: &RemoteState, device: &str) -> bool {
    state.connection != ProviderConnectionState::Connected
        || state.authentication_required
        || active_context_rows_conflict(&state.devices, device)
        || state.playback.as_ref().is_some_and(|frame| {
            frame
                .device
                .as_ref()
                .is_none_or(|current| current.id != device || current.is_restricted)
        })
}

fn active_context_device_is_current(state: &RemoteState, device: &str) -> bool {
    let mut matching = state.devices.iter().filter(|known| known.id == device);
    !active_context_state_conflicts(state, device)
        && matching.next().is_some_and(|known| !known.is_restricted)
        && matching.next().is_none()
}

fn active_context_clock(clock: &RemoteOutputClock) -> bool {
    clock.active
        && clock.media_id.is_some()
        && clock.metadata.is_some()
        && clock.queue_epoch.is_some_and(|epoch| epoch > 0)
        && clock.control_uptime_ms.is_some()
        && clock.rate.is_finite()
        && (clock.state == PlaybackState::Paused && clock.rate == 0.0
            || clock.state == PlaybackState::Playing && clock.rate > 0.0 && clock.rate <= 4.0)
}

struct CaptureBinding {
    witness: OwnedCaptureWitness,
    operation: u64,
    metadata: RemoteOutputMetadata,
    device: PlaybackDevice,
}

fn capture_device_is_current(state: &RemoteState, device: &str) -> bool {
    !state.authentication_required
        && matches!(
            state.connection,
            ProviderConnectionState::Connected | ProviderConnectionState::Unavailable
        )
        && state
            .devices
            .iter()
            .any(|known| known.id == device && !known.is_restricted)
        && !state
            .devices
            .iter()
            .any(|known| known.id != device && known.is_active)
        && state
            .playback
            .as_ref()
            .and_then(|frame| frame.device.as_ref())
            .is_none_or(|current| {
                current.id == device && !current.is_restricted
                    || current.id != device && !current.is_active
            })
}

// Retention is separate from promotion. A disappearing /devices row is not a
// takeover while this exact proven occurrence still supplies a fresh local
// clock. Explicit cloud contradictions revoke the descriptor immediately.
fn retained_capture_device(
    state: &RemoteState,
    previous: &PlaybackDevice,
) -> Option<PlaybackDevice> {
    if state.authentication_required
        || !matches!(
            state.connection,
            ProviderConnectionState::Connected | ProviderConnectionState::Unavailable
        )
        || previous.is_restricted
        || state.devices.iter().any(|device| {
            device.id != previous.id && (device.is_active || device.name == previous.name)
                || device.id == previous.id
                    && (device.is_restricted || device.name != previous.name)
        })
        || state.playback.as_ref().is_some_and(|frame| {
            frame.device.as_ref().is_none_or(|device| {
                device.id != previous.id || device.is_restricted || device.name != previous.name
            })
        })
    {
        return None;
    }
    let mut matching = state
        .devices
        .iter()
        .filter(|device| device.id == previous.id);
    let current = matching.next();
    if matching.next().is_some() {
        return None;
    }
    Some(current.unwrap_or(previous).clone())
}

impl CaptureBinding {
    fn matches_clock(&self, clock: &RemoteOutputClock) -> bool {
        let proof = &self.witness;
        clock.active
            && clock.generation == proof.generation
            && clock.device_id == proof.device_id
            && clock.media_id.as_ref() == Some(&proof.media_id)
            && clock.queue_epoch == Some(proof.queue_epoch)
            && clock.sequence >= proof.playing_sequence
            && clock
                .control_uptime_ms
                .is_some_and(|uptime| uptime >= proof.playing_control_uptime_ms)
            && clock
                .metadata
                .as_ref()
                .is_none_or(|metadata| metadata.context_uri.as_ref() == Some(&proof.context_uri))
            && ((clock.state == PlaybackState::Playing && clock.rate > 0.0)
                || (clock.state == PlaybackState::Paused && clock.rate == 0.0)
                || (clock.state == PlaybackState::Playing
                    && clock.rate == 0.0
                    && clock.actions.is_none()))
    }
}

// A context can start with the same URI that an inactive device last played.
// A mere activation is ambiguous: require a new, full buffering occurrence
// which rewinds that inactive item, then a full playing clock after HTTP ACK.
// This only advances selection visibility; it never grants a control binding.
struct InactiveContextRestart {
    media: MediaId,
    sequence: u64,
    uptime: u64,
    queue_epoch: u64,
    position_ms: u64,
    loading: Option<InactiveContextLoading>,
}

struct InactiveContextLoading {
    sequence: u64,
    uptime: u64,
    queue_epoch: u64,
    observed_at: Instant,
}

impl InactiveContextRestart {
    fn from_origin(clock: &RemoteOutputClock, target: &SelectionTarget) -> Option<Self> {
        if !matches!(target, SelectionTarget::Context(_))
            || clock.active
            || clock.state != PlaybackState::Paused
            || clock.rate != 0.0
            || clock.actions.is_none()
            || clock.metadata.is_none()
            || clock.confirmed_same_media_next.is_some()
        {
            return None;
        }
        Some(Self {
            media: clock.media_id.clone()?,
            sequence: clock.sequence,
            uptime: clock.control_uptime_ms?,
            queue_epoch: clock.queue_epoch.filter(|epoch| *epoch > 0)?,
            position_ms: clock.position_ms,
            loading: None,
        })
    }

    fn observe(
        &mut self,
        clock: &RemoteOutputClock,
        target: &SelectionTarget,
        now: Instant,
        confirmed: bool,
    ) -> bool {
        if clock.sequence <= self.sequence
            || clock.media_id.as_ref() != Some(&self.media)
            || clock.confirmed_same_media_next.is_some()
            || clock
                .control_uptime_ms
                .is_none_or(|uptime| uptime < self.uptime)
        {
            return false;
        }
        if let Some(loading) = &self.loading
            && (!clock.active
                || (!confirmed && clock.state == PlaybackState::Paused)
                || clock.queue_epoch != Some(loading.queue_epoch)
                || clock
                    .control_uptime_ms
                    .is_none_or(|uptime| uptime < loading.uptime))
        {
            return false;
        }
        // Granular activation events have no actions; they cannot establish a
        // new occurrence or replace its full Loading witness.
        if clock.active && clock.actions.is_some() && clock.metadata.is_some() {
            if !target.matches(clock) {
                return false;
            }
            if clock.state == PlaybackState::Loading && clock.rate == 0.0 {
                let Some(epoch) = clock.queue_epoch.filter(|epoch| *epoch > self.queue_epoch)
                else {
                    return false;
                };
                if clock.position_ms > RESUME_POSITION_SLOP_MS
                    || clock.position_ms.saturating_add(RESUME_POSITION_SLOP_MS) >= self.position_ms
                {
                    return false;
                }
                if self.loading.is_none() {
                    self.loading = Some(InactiveContextLoading {
                        sequence: clock.sequence,
                        uptime: clock.control_uptime_ms.expect("validated restart uptime"),
                        queue_epoch: epoch,
                        observed_at: now,
                    });
                }
            }
        }
        true
    }

    fn confirms(&self, clock: &RemoteOutputClock, observed_at: Instant) -> bool {
        self.loading.as_ref().is_some_and(|loading| {
            clock.active
                && clock.state == PlaybackState::Playing
                && clock.rate > 0.0
                && clock.actions.is_some()
                && clock.metadata.is_some()
                && clock.media_id.as_ref() == Some(&self.media)
                && clock.confirmed_same_media_next.is_none()
                && clock.sequence > loading.sequence
                && clock.queue_epoch == Some(loading.queue_epoch)
                && clock
                    .control_uptime_ms
                    .is_some_and(|uptime| uptime >= loading.uptime)
                && observed_at >= loading.observed_at
                && u128::from(clock.position_ms)
                    <= observed_at
                        .saturating_duration_since(loading.observed_at)
                        .as_millis()
                        + u128::from(RESUME_POSITION_SLOP_MS)
        })
    }
}

pub(super) struct ContextResumeReceipt {
    pub operation: u64,
    pub dispatched_at: Instant,
}

struct ContextResumeLoading {
    generation: u64,
    sequence: u64,
    uptime: u64,
    observed_at: Instant,
    queue_epoch: Option<u64>,
}

struct ContextResume {
    operation: u64,
    device: String,
    item: MediaId,
    context: String,
    position_ms: u64,
    sent_at: Instant,
    generation: Option<u64>,
    loading: Option<ContextResumeLoading>,
    dispatched_at: Option<Instant>,
    published: bool,
}

impl ContextResume {
    fn matches(&self, clock: &RemoteOutputClock) -> bool {
        clock.media_id.as_ref() == Some(&self.item)
            && clock
                .metadata
                .as_ref()
                .and_then(|m| m.context_uri.as_deref())
                == Some(&self.context)
    }

    fn observe(&mut self, clock: &RemoteOutputClock, now: Instant) -> bool {
        if clock.device_id != self.device
            || self
                .generation
                .is_some_and(|generation| clock.generation != generation)
        {
            return false;
        }
        if self.loading.is_some() && (!clock.active || clock.state == PlaybackState::Paused) {
            return false;
        }
        if clock.active && clock.actions.is_some() && clock.metadata.is_some() {
            if !self.matches(clock) {
                return false;
            }
            if let Some(uptime) = clock.control_uptime_ms
                && clock.state == PlaybackState::Loading
                && clock.rate == 0.0
                && clock.position_ms.abs_diff(self.position_ms) <= RESUME_POSITION_SLOP_MS
            {
                self.generation = Some(clock.generation);
                self.loading = Some(ContextResumeLoading {
                    generation: clock.generation,
                    sequence: clock.sequence,
                    uptime,
                    observed_at: now,
                    queue_epoch: clock.queue_epoch,
                });
            }
        }
        true
    }

    fn confirms(&self, clock: &RemoteOutputClock, observed_at: Instant) -> bool {
        let Some((loading, dispatched_at)) = self.loading.as_ref().zip(self.dispatched_at) else {
            return false;
        };
        let elapsed_ms = observed_at
            .saturating_duration_since(dispatched_at)
            .as_millis() as u64;
        loading.observed_at >= dispatched_at
            && observed_at >= self.sent_at
            && clock.active
            && clock.generation == loading.generation
            && clock.device_id == self.device
            && clock.sequence > loading.sequence
            && clock
                .control_uptime_ms
                .is_some_and(|uptime| uptime >= loading.uptime)
            && clock.state == PlaybackState::Playing
            && clock.rate > 0.0
            && clock.actions.is_some()
            && self.matches(clock)
            && clock.position_ms >= self.position_ms.saturating_sub(RESUME_POSITION_SLOP_MS)
            && clock.position_ms
                <= self
                    .position_ms
                    .saturating_add(elapsed_ms)
                    .saturating_add(RESUME_POSITION_SLOP_MS)
    }
}

struct ResumePause {
    operation: u64,
    generation: u64,
    device: String,
    item: MediaId,
    context: String,
    queue_epoch: u64,
    expires: Instant,
    revoked: bool,
    dispatch: Option<ResumePauseDispatch>,
}

struct ResumePauseDispatch {
    command: RemoteOutputCommand,
    pending: bool,
    acknowledged: bool,
    playing_position_ms: u64,
    stationary_position: Option<(u64, Instant)>,
}

enum ResumePauseObservation {
    Complete,
    Provisional(u64),
    Retained(u64),
}

impl ResumePause {
    fn matches(&self, clock: &RemoteOutputClock) -> bool {
        clock.active
            && clock.generation == self.generation
            && clock.device_id == self.device
            && clock.media_id.as_ref() == Some(&self.item)
            && clock.queue_epoch == Some(self.queue_epoch)
            && clock
                .metadata
                .as_ref()
                .and_then(|m| m.context_uri.as_deref())
                == Some(&self.context)
    }

    fn current(
        &self,
        state: Option<&RemoteState>,
        clock: Option<(&RemoteOutputClock, Instant)>,
        now: Instant,
    ) -> bool {
        !self.revoked
            && now < self.expires
            && state.is_some_and(|state| context_resume_device(state, &self.device).is_some())
            && clock.is_some_and(|(clock, at)| {
                now.saturating_duration_since(at) <= CONTROL_FRESHNESS
                    && clock.control_uptime_ms.is_some()
                    && self.matches(clock)
            })
    }
}

fn context_resume_device<'a>(state: &'a RemoteState, id: &str) -> Option<&'a PlaybackDevice> {
    if state.connection != ProviderConnectionState::Connected
        || state.authentication_required
        || state
            .devices
            .iter()
            .any(|device| device.id != id && device.is_active)
        || state
            .playback
            .as_ref()
            .and_then(|frame| frame.device.as_ref())
            .is_some_and(|device| {
                device.id == id && device.is_restricted || device.id != id && device.is_active
            })
    {
        return None;
    }
    state
        .devices
        .iter()
        .find(|device| device.id == id && !device.is_restricted)
}

pub(super) struct OutputProjection {
    base: Option<RemoteState>,
    known: Option<RemotePlayback>,
    chapter: Option<MediaItem>,
    selected_chapter: Option<MediaItem>,
    base_at: Instant,
    anchor_at: Instant,
    raw_observation: Option<u64>,
    serial: u64,
    watermark: (u64, u64),
    clock: Option<(RemoteOutputClock, Instant)>,
    pending_clock_at: Option<Instant>,
    refresh_key: Option<(u64, String, Option<MediaId>, bool, bool)>,
    binding: Option<(u64, String)>,
    control_media: Option<MediaId>,
    // Denial only: losing a local clock must not redirect a relative command
    // to whichever device Web API currently considers active. This carries no
    // media, clock, capability or permission and has no presentation role.
    inert_owned_destination: Option<String>,
    navigation_actions: Option<(bool, bool, Instant)>,
    queue: Option<(RemoteOutputQueue, Instant)>,
    queue_watermark: (u64, u64),
    local_control_result: Option<RemoteControlResult>,
    http_selection: Option<HttpSelection>,
    capture_binding: Option<CaptureBinding>,
    capture_serial: u64,
    context_resume: Option<ContextResume>,
    resume_pause: Option<ResumePause>,
    dirty: bool,
    rate_limit: Option<RateLimit>,
}
impl Default for OutputProjection {
    fn default() -> Self {
        Self {
            base: None,
            known: None,
            chapter: None,
            selected_chapter: None,
            base_at: Instant::now(),
            anchor_at: Instant::now(),
            raw_observation: None,
            serial: 0,
            watermark: (0, 0),
            clock: None,
            pending_clock_at: None,
            refresh_key: None,
            binding: None,
            control_media: None,
            inert_owned_destination: None,
            navigation_actions: None,
            queue: None,
            queue_watermark: (0, 0),
            local_control_result: None,
            http_selection: None,
            capture_binding: None,
            capture_serial: 0,
            context_resume: None,
            resume_pause: None,
            dirty: false,
            rate_limit: None,
        }
    }
}
impl OutputProjection {
    pub fn reset(&mut self) {
        self.rate_limit = None;
        self.base = None;
        self.known = None;
        self.chapter = None;
        self.selected_chapter = None;
        self.raw_observation = None;
        self.clock = None;
        self.pending_clock_at = None;
        self.refresh_key = None;
        self.clear_binding();
        self.control_media = None;
        self.inert_owned_destination = None;
        self.queue = None;
        self.local_control_result = None;
        self.http_selection = None;
        self.capture_binding = None;
        self.context_resume = None;
        self.resume_pause = None;
        self.dirty = false;
    }
    fn clear_binding(&mut self) {
        self.binding = None;
        self.navigation_actions = None;
    }
    pub fn control_clock(&self, now: Instant) -> Option<&RemoteOutputClock> {
        self.navigation_clock(now)
            .filter(|clock| clock.media_id == self.control_media)
    }
    pub(super) fn owned_capture_witness(&self, now: Instant) -> Option<OwnedCaptureWitness> {
        let binding = self.capture_binding.as_ref()?;
        let (clock, at) = self.clock.as_ref()?;
        (now.saturating_duration_since(*at) <= CONTROL_FRESHNESS
            && retained_capture_device(self.base.as_ref()?, &binding.device).is_some()
            && binding.matches_clock(clock))
        .then(|| binding.witness.clone())
    }
    fn capture_device(&self, state: &RemoteState, now: Instant) -> Option<PlaybackDevice> {
        let binding = self.capture_binding.as_ref()?;
        let (clock, at) = self.clock.as_ref()?;
        if self.binding.is_some()
            || now.saturating_duration_since(*at) > CONTROL_FRESHNESS
            || retained_capture_device(state, &binding.device).is_none()
            || !binding.matches_clock(clock)
        {
            return None;
        }
        if state.playback.as_ref().is_some_and(|frame| {
            frame.device.as_ref().is_some_and(|device| {
                device.id == binding.witness.device_id && !device.is_restricted
            }) && frame.current.as_ref().is_some_and(|item| {
                super::mapping::same_playback_id(&item.id, &binding.witness.media_id)
            })
        }) {
            // The ordinary exact HTTP match may acquire its normal binding.
            return None;
        }
        // Preserve display of this stronger proven occurrence through a cloud
        // 204 and beyond the short selection-witness TTL. The authenticated
        // clock supplies active state; this never creates a control binding.
        retained_capture_device(state, &binding.device)
    }
    pub(super) fn control_clock_age(&self, now: Instant) -> Option<Duration> {
        self.control_clock(now)?;
        self.clock
            .as_ref()
            .map(|(_, at)| now.saturating_duration_since(*at))
    }
    pub fn navigation_clock(&self, now: Instant) -> Option<&RemoteOutputClock> {
        let (clock, at) = self.clock.as_ref()?;
        let bound = self.binding.as_ref().is_some_and(|(generation, device)| {
            *generation == clock.generation && *device == clock.device_id
        });
        let row_witness = self.http_selection.as_ref().is_some_and(|selection| {
            matches!(selection.target, SelectionTarget::ContextRow { .. })
                && selection.confirmed_media.as_ref() == clock.media_id.as_ref()
                && selection.confirmed_media.is_some()
        });
        (bound
            && !row_witness
            && clock.active
            && clock.control_uptime_ms.is_some()
            && clock.media_id.is_some()
            && now.saturating_duration_since(*at) <= CONTROL_FRESHNESS)
            .then_some(clock)
    }
    /// A validated row witness can scope a Web API request while the ordinary
    /// Web API playback frame still describes the preceding track.
    pub(super) fn http_navigation_clock(
        &self,
        now: Instant,
    ) -> Option<(&RemoteOutputClock, Instant)> {
        let (clock, at) = self.clock.as_ref()?;
        let base = self.base.as_ref()?;
        if base.authentication_required
            || !clock.active
            || clock.control_uptime_ms.is_none()
            || now.saturating_duration_since(*at) > CONTROL_FRESHNESS
            || base
                .playback
                .as_ref()
                .and_then(|p| p.device.as_ref())
                .is_some_and(|device| {
                    device.id == clock.device_id && device.is_restricted
                        || device.id != clock.device_id && device.is_active
                })
        {
            return None;
        }
        let row_witness = self.http_selection.as_ref().is_some_and(|selection| {
            selection.generation == clock.generation
                && selection.device == clock.device_id
                && matches!(selection.target, SelectionTarget::ContextRow { .. })
                && selection.confirmed_media.is_some()
                && selection.confirmed_media == clock.media_id
                && selection.target.matches(clock)
                && now.saturating_duration_since(selection.sent_at) < SELECTION_EXPIRY
                && base.connection == ProviderConnectionState::Connected
        });
        (self.navigation_clock(now).is_some() || row_witness).then_some((clock, *at))
    }
    pub(super) fn pending_http_row(
        &self,
        clock: &RemoteOutputClock,
    ) -> Option<super::owned_http::Row> {
        let selection = self.http_selection.as_ref()?;
        if selection.generation == clock.generation
            && selection.device == clock.device_id
            && let SelectionTarget::ContextRow { uri, item } = &selection.target
        {
            return Some(super::owned_http::Row {
                media: item.clone(),
                context: uri.clone(),
            });
        }
        None
    }
    pub(super) fn observed_at(&self) -> Option<(&RemoteOutputClock, Instant)> {
        self.clock.as_ref().map(|(clock, at)| (clock, *at))
    }
    pub fn has_owned_binding(&self) -> bool {
        self.binding.is_some()
    }
    pub(super) fn observed_clock(&self) -> Option<&RemoteOutputClock> {
        self.clock.as_ref().map(|(clock, _)| clock)
    }
    pub fn can_project_owned_metadata(&self) -> bool {
        self.has_owned_binding()
            && self
                .clock
                .as_ref()
                .is_some_and(|(clock, _)| clock.metadata.is_some())
    }
    fn preserves_confirmed_restart_pause(&self, control: &RemoteControl, now: Instant) -> bool {
        let RemoteControl::PauseOnDevice { device_id } = control else {
            return false;
        };
        let Some(selection) = &self.http_selection else {
            return false;
        };
        let Some(confirmed) = &selection.confirmed_media else {
            return false;
        };
        let Some(restart) = &selection.inactive_restart else {
            return false;
        };
        let Some(loading) = &restart.loading else {
            return false;
        };
        let Some((clock, at)) = &self.clock else {
            return false;
        };
        self.binding.is_none()
            && matches!(selection.target, SelectionTarget::Context(_))
            && &selection.device == device_id
            && selection.prior_context_media.as_ref() == Some(confirmed)
            && &restart.media == confirmed
            && now.saturating_duration_since(selection.sent_at) < SELECTION_EXPIRY
            && now.saturating_duration_since(*at) <= CONTROL_FRESHNESS
            && self.base.as_ref().is_some_and(|state| {
                state.connection == ProviderConnectionState::Connected
                    && !state.authentication_required
                    && state
                        .devices
                        .iter()
                        .any(|device| device.id == *device_id && !device.is_restricted)
            })
            && clock.active
            && clock.generation == selection.generation
            && clock.device_id == selection.device
            && clock.media_id.as_ref() == Some(confirmed)
            && selection.target.matches(clock)
            && clock.actions.is_some()
            && clock.queue_epoch == Some(loading.queue_epoch)
            && clock.sequence > loading.sequence
            && selection
                .accepted
                .as_ref()
                .is_some_and(|(after, _)| clock.sequence > *after)
            && clock
                .control_uptime_ms
                .is_some_and(|uptime| uptime >= loading.uptime)
            && clock.confirmed_same_media_next.is_none()
            && ((clock.state == PlaybackState::Playing && clock.rate > 0.0)
                || (clock.state == PlaybackState::Paused && clock.rate == 0.0))
    }
    pub fn remember_selection(&mut self, control: &RemoteControl, now: Instant) {
        if matches!(
            control,
            RemoteControl::Start { .. }
                | RemoteControl::StartOnDevice { .. }
                | RemoteControl::StartSpokenOnDevice { .. }
                | RemoteControl::ResumeOnDevice { .. }
                | RemoteControl::StartContext { .. }
                | RemoteControl::Transfer { .. }
                | RemoteControl::Next
                | RemoteControl::Previous
                | RemoteControl::Seek { .. }
                | RemoteControl::Pause
        ) || matches!(control, RemoteControl::PauseOnDevice { device_id }
                if self.capture_binding.as_ref().is_some_and(|binding| binding.witness.device_id != *device_id))
        {
            self.capture_binding = None;
        }
        // An addressed Pause changes the transport of this already confirmed
        // occurrence. Keep its display witness while HTTP catches up, without
        // granting the binding or native-control capability it still lacks.
        if !self.preserves_confirmed_restart_pause(control, now) {
            self.http_selection = None;
        }
        self.context_resume = None;
        if let Some(pause) = &mut self.resume_pause {
            pause.revoked = true;
        }
        match control {
            RemoteControl::Start { item }
            | RemoteControl::StartOnDevice { item, .. }
            | RemoteControl::StartSpokenOnDevice { item, .. }
            | RemoteControl::ResumeOnDevice { item, .. } => {
                self.selected_chapter = item
                    .id
                    .as_str()
                    .starts_with("spotify:chapter:")
                    .then(|| item.clone());
            }
            RemoteControl::StartContext { .. } | RemoteControl::Transfer { .. } => {
                self.selected_chapter = None
            }
            _ => (),
        }
    }
    pub(super) fn remember_http_selection(
        &mut self,
        control: &RemoteControl,
        now: Instant,
    ) -> Option<super::DirectOwnedStart> {
        let (device, operation, target) = match control {
            RemoteControl::StartOnDevice {
                item,
                device_id,
                operation_id: Some(operation),
            } if item.id.as_str().starts_with("spotify:track:") => (
                device_id,
                *operation,
                SelectionTarget::Track(item.id.clone()),
            ),
            RemoteControl::StartContext {
                uri,
                device_id,
                operation_id: Some(operation),
                item: None,
                ..
            } if uri.starts_with("spotify:album:") || uri.starts_with("spotify:playlist:") => {
                (device_id, *operation, SelectionTarget::Context(uri.clone()))
            }
            RemoteControl::StartContext {
                uri,
                device_id,
                operation_id: Some(operation),
                item: Some(item),
                position: Some(_),
                query_id: Some(_),
            } if uri.starts_with("spotify:album:") || uri.starts_with("spotify:playlist:") => (
                device_id,
                *operation,
                SelectionTarget::ContextRow {
                    uri: uri.clone(),
                    item: item.id.clone(),
                },
            ),
            RemoteControl::ResumeOnDevice {
                device_id,
                operation_id: Some(operation),
                item,
                position_ms,
                context_uri: Some(context),
            } if item.id.as_str().starts_with("spotify:track:")
                && (context.starts_with("spotify:album:")
                    || context.starts_with("spotify:playlist:")) =>
            {
                let device = context_resume_device(self.base.as_ref()?, device_id)?;
                let prior = self
                    .clock
                    .as_ref()
                    .filter(|(_, at)| now.saturating_duration_since(*at) <= CONTROL_FRESHNESS);
                if prior.is_some_and(|(clock, _)| {
                    clock.device_id != device.id
                        || clock.active && clock.state == PlaybackState::Playing
                }) {
                    return None;
                }
                self.context_resume = Some(ContextResume {
                    operation: *operation,
                    device: device.id.clone(),
                    item: item.id.clone(),
                    context: context.clone(),
                    position_ms: *position_ms,
                    sent_at: now,
                    generation: prior.map(|(clock, _)| clock.generation),
                    loading: None,
                    dispatched_at: None,
                    published: false,
                });
                // Resume always uses its own exact targeted write without a transfer.
                return None;
            }
            _ => return None,
        };
        let (clock, at) = self.clock.as_ref()?;
        if (self.binding.is_some() && !matches!(target, SelectionTarget::ContextRow { .. }))
            || &clock.device_id != device
            || clock.control_uptime_ms.is_none()
            || now.saturating_duration_since(*at) > CONTROL_FRESHNESS
            || !self.base.as_ref().is_some_and(|state| {
                state.connection == ProviderConnectionState::Connected
                    && !state.authentication_required
                    && state
                        .devices
                        .iter()
                        .any(|known| &known.id == device && !known.is_restricted)
            })
        {
            return None;
        }
        let prior_context_media = match &target {
            SelectionTarget::Context(_) | SelectionTarget::ContextRow { .. } => {
                clock.media_id.clone()
            }
            SelectionTarget::Track(_) => None,
        };
        // Positioned rows already send a single exact targeted write and
        // must never use the Transfer fallback for whole-context starts.
        // A bootstrap can make this exact owned endpoint active before the
        // cloud playback row catches up. Its addressed HTTP Play does not need
        // to transfer the previous cloud selection first. Device-change clocks
        // intentionally omit actions: this path grants no native capability.
        let active_context_origin = (matches!(target, SelectionTarget::Context(_))
            && active_context_clock(clock)
            && self
                .base
                .as_ref()
                .is_some_and(|state| active_context_device_is_current(state, device)))
        .then(|| ActiveContextStart {
            media: clock.media_id.clone().unwrap(),
            context: clock.metadata.as_ref().unwrap().context_uri.clone(),
            epoch: clock.queue_epoch.unwrap(),
            sequence: clock.sequence,
            uptime: clock.control_uptime_ms.unwrap(),
            expires: now + SELECTION_EXPIRY,
            revoked: false,
        });
        let direct_first = (!clock.active && !matches!(target, SelectionTarget::ContextRow { .. }))
            || active_context_origin.is_some();
        let inactive_restart = InactiveContextRestart::from_origin(clock, &target);
        self.http_selection = Some(HttpSelection {
            operation,
            generation: clock.generation,
            device: device.clone(),
            target,
            prior_context_media,
            sent_at: now,
            accepted: None,
            confirmed_media: None,
            inactive_restart,
            capture_promoted: false,
        });
        direct_first.then(|| super::DirectOwnedStart {
            operation,
            generation: clock.generation,
            device: device.clone(),
            observed_at: Some(*at),
            active_context_origin,
        })
    }

    pub(super) fn accept_context_resume(&mut self, receipt: ContextResumeReceipt) {
        if let Some(resume) = &mut self.context_resume
            && resume.operation == receipt.operation
            && receipt.dispatched_at >= resume.sent_at
        {
            resume.dispatched_at = Some(receipt.dispatched_at);
        }
    }

    pub(super) fn cancel_context_resume(&mut self, operation: u64, device: &str) {
        if self
            .context_resume
            .as_ref()
            .is_some_and(|resume| resume.operation == operation && resume.device == device)
        {
            self.context_resume = None;
            self.dirty = true;
        }
    }

    pub(super) fn resume_pause_clock(
        &self,
        operation: u64,
        device: &str,
        now: Instant,
    ) -> Result<Option<&RemoteOutputClock>, PlayerError> {
        let Some(pause) = &self.resume_pause else {
            return Ok(None);
        };
        if pause.operation != operation && pause.revoked {
            // A later Resume may use the ordinary addressed route, including
            // standalone starts which have no exact local context witness.
            return Ok(None);
        }
        if pause.operation != operation
            || pause.device != device
            || pause.dispatch.is_some()
            || !pause.current(self.base.as_ref(), self.observed_at(), now)
        {
            return Err(super::invalid(
                "The resumed playback changed before it could be paused",
            ));
        }
        let clock = self.observed_clock().expect("current scoped resume clock");
        if clock.state != PlaybackState::Playing || clock.rate <= 0.0 {
            return Err(super::invalid(
                "The resumed playback changed before it could be paused",
            ));
        }
        Ok(clock
            .actions
            .as_ref()
            .is_some_and(|actions| actions.pause)
            .then_some(clock))
    }

    pub(super) fn reserve_resume_pause(&mut self, command: RemoteOutputCommand, now: Instant) {
        let playing_position_ms = self
            .observed_clock()
            .expect("scoped resume pause clock")
            .position_ms;
        let pause = self
            .resume_pause
            .as_mut()
            .expect("scoped resume pause proof");
        pause.expires = now + RESUME_PAUSE_WINDOW;
        pause.dispatch = Some(ResumePauseDispatch {
            command,
            pending: true,
            acknowledged: false,
            playing_position_ms,
            stationary_position: None,
        });
    }

    pub(super) fn resume_pause_ready(&self, request: u64) -> bool {
        self.resume_pause
            .as_ref()
            .and_then(|pause| pause.dispatch.as_ref())
            .is_some_and(|pause| pause.pending && pause.command.request_id == Some(request))
    }

    pub(super) fn resume_pause_in_flight(&mut self, now: Instant) -> Option<(u64, u64)> {
        let current = self
            .resume_pause
            .as_ref()
            .is_some_and(|pause| pause.current(self.base.as_ref(), self.observed_at(), now));
        let pause = self.resume_pause.as_mut()?;
        if !current {
            pause.revoked = true;
        }
        let dispatch = pause.dispatch.as_ref()?;
        current.then_some((dispatch.command.id, dispatch.command.generation))
    }

    pub(super) fn revoked_resume_pause(&self) -> Option<(u64, u64)> {
        let pause = self.resume_pause.as_ref().filter(|pause| pause.revoked)?;
        let command = &pause.dispatch.as_ref()?.command;
        Some((command.id, command.generation))
    }

    pub(super) fn revoke_resume_pause_proof(&mut self) {
        if let Some(pause) = &mut self.resume_pause {
            pause.revoked = true;
        }
    }

    pub(super) fn take_resume_pause(&mut self, now: Instant) -> Option<RemoteUpdate> {
        let authorized = self.resume_pause_in_flight(now).is_some()
            && self.observed_clock().is_some_and(|clock| {
                clock.state == PlaybackState::Playing
                    && clock.rate > 0.0
                    && clock.actions.as_ref().is_some_and(|actions| actions.pause)
            });
        let pause = self.resume_pause.as_mut()?;
        let dispatch = pause.dispatch.as_mut()?;
        if !std::mem::take(&mut dispatch.pending) {
            return None;
        }
        if !authorized || now >= pause.expires {
            pause.revoked = true;
            return Some(RemoteUpdate::OutputCommandFailed {
                control: RemoteOutputControl::Pause,
                error: super::invalid("The resumed playback changed before it could be paused"),
            });
        }
        if let Some(event) = crate::autoplay_trace::Event::discovery(
            crate::autoplay_trace::DiscoveryStage::ResumePauseNative {
                operation: pause.operation,
                generation: dispatch.command.generation,
            },
        ) {
            event.write();
        }
        Some(RemoteUpdate::OutputCommand(dispatch.command.clone()))
    }

    pub(super) fn acknowledge_resume_pause(&mut self, id: u64, generation: u64, accepted: bool) {
        let Some(pause) = &mut self.resume_pause else {
            return;
        };
        let Some(dispatch) = &mut pause.dispatch else {
            return;
        };
        if dispatch.pending
            || dispatch.acknowledged
            || dispatch.command.id != id
            || dispatch.command.generation != generation
        {
            return;
        }
        if accepted && !pause.revoked {
            dispatch.acknowledged = true;
            if let Some((clock, at)) = &self.clock
                && clock.state == PlaybackState::Paused
            {
                self.pending_clock_at = Some(*at);
            }
            self.dirty = true;
        } else {
            pause.revoked = true;
        }
    }

    fn witnessed_resume_pause(
        &mut self,
        state: &RemoteState,
        now: Instant,
    ) -> Option<(PlaybackDevice, ResumePauseObservation)> {
        if self.has_owned_binding() {
            return None;
        }
        let pause = self.resume_pause.as_mut()?;
        let (clock, at) = self.clock.as_ref()?;
        // The authenticated clock can show the effect before its pipe ACK.
        // Keep waiting for that ACK independently of this read-only projection.
        if !pause.current(Some(state), Some((clock, *at)), now)
            || clock.state != PlaybackState::Paused
            || clock.rate != 0.0
        {
            return None;
        }
        let dispatch = pause
            .dispatch
            .as_mut()
            .filter(|dispatch| !dispatch.pending && clock.sequence > dispatch.command.sequence)?;
        let observation = if clock.actions.is_some() {
            dispatch.stationary_position = Some((clock.position_ms, *at));
            ResumePauseObservation::Complete
        } else if let Some((position_ms, stationary_at)) = dispatch.stationary_position
            && now.saturating_duration_since(stationary_at) <= CONTROL_FRESHNESS
        {
            ResumePauseObservation::Retained(position_ms)
        } else {
            // Soloist's playback_changed event has no measured position. Keep the
            // card visible without publishing a new checkpoint or owned observation.
            let position_ms = dispatch
                .stationary_position
                .map_or(dispatch.playing_position_ms, |(position_ms, _)| position_ms);
            ResumePauseObservation::Provisional(position_ms)
        };
        Some((
            context_resume_device(state, &pause.device)?.clone(),
            observation,
        ))
    }

    fn witnessed_context_resume(
        &mut self,
        state: &RemoteState,
        now: Instant,
    ) -> Option<PlaybackDevice> {
        let resume = self.context_resume.as_mut()?;
        let device = context_resume_device(state, &resume.device);
        if now.saturating_duration_since(resume.sent_at) >= SELECTION_EXPIRY
            || device.is_none()
            || state.playback.as_ref().is_some_and(|frame| {
                frame
                    .device
                    .as_ref()
                    .is_some_and(|device| device.id == resume.device)
                    && frame
                        .current
                        .as_ref()
                        .is_some_and(|item| item.id == resume.item)
                    && frame.context_uri.as_deref() == Some(resume.context.as_str())
                    && frame.state == PlaybackState::Playing
            })
        {
            self.context_resume = None;
            return None;
        }
        let (clock, observed_at) = self.clock.as_ref()?;
        if now.saturating_duration_since(*observed_at) > CONTROL_FRESHNESS
            || !resume.confirms(clock, *observed_at)
        {
            return None;
        }
        if !resume.published {
            resume.published = true;
            if let Some(queue_epoch) = clock.queue_epoch.filter(|&epoch| epoch > 0)
                && resume
                    .loading
                    .as_ref()
                    .and_then(|loading| loading.queue_epoch)
                    == Some(queue_epoch)
            {
                self.resume_pause = Some(ResumePause {
                    operation: resume.operation,
                    generation: clock.generation,
                    device: resume.device.clone(),
                    item: resume.item.clone(),
                    context: resume.context.clone(),
                    queue_epoch,
                    expires: now + RESUME_PAUSE_WINDOW,
                    revoked: false,
                    dispatch: None,
                });
            }
            self.pending_clock_at = Some(*observed_at);
            if let Some(event) = crate::autoplay_trace::Event::discovery(
                crate::autoplay_trace::DiscoveryStage::ContextResumeProjected {
                    operation: resume.operation,
                    generation: clock.generation,
                },
            ) {
                event.write();
            }
        }
        device.cloned()
    }

    pub(super) fn recheck_direct_owned_start(&self, start: &mut super::DirectOwnedStart) {
        let observed = self.clock.as_ref().and_then(|(clock, at)| {
            (clock.generation == start.generation
                && clock.device_id == start.device
                && clock.control_uptime_ms.is_some()
                && self.base.as_ref().is_some_and(|state| {
                    state.connection == ProviderConnectionState::Connected
                        && !state.authentication_required
                        && state
                            .devices
                            .iter()
                            .any(|device| device.id == start.device && !device.is_restricted)
                })
                && start.active_context_origin.as_ref().is_none_or(|origin| {
                    active_context_clock(clock)
                        && origin.matches(clock)
                        && self.base.as_ref().is_some_and(|state| {
                            active_context_device_is_current(state, &start.device)
                        })
                        && self.http_selection.as_ref().is_some_and(|selection| {
                            selection.operation == start.operation
                                && selection.generation == start.generation
                                && selection.device == start.device
                        })
                }))
            .then_some(*at)
        });
        if observed.is_none()
            && let Some(origin) = &mut start.active_context_origin
        {
            origin.revoke();
        }
        start.observed_at = observed;
    }

    fn observe_selection_result(&mut self, state: &RemoteState) {
        let (Some(selection), Some(result)) = (&mut self.http_selection, &state.control_result)
        else {
            return;
        };
        if result.operation_id != selection.operation {
            return;
        }
        if result.error.is_some() {
            self.http_selection = None;
            return;
        }
        if selection.accepted.is_none()
            && let Some((clock, _)) = &self.clock
            && clock.generation == selection.generation
            && clock.device_id == selection.device
        {
            // Spotify may transiently activate the prior item before the HTTP
            // acknowledgement. Require a later observation of the new start.
            let prior = if clock.state == PlaybackState::Playing
                && clock.rate > 0.0
                && selection.target.matches(clock)
            {
                clock.media_id.clone()
            } else {
                None
            };
            selection.accepted = Some((clock.sequence, prior));
        }
    }

    fn selection_device(&mut self, state: &RemoteState, now: Instant) -> Option<PlaybackDevice> {
        let selection = self.http_selection.as_mut()?;
        let positioned_row = matches!(selection.target, SelectionTarget::ContextRow { .. });
        if (self.binding.is_some() && !positioned_row)
            || now.saturating_duration_since(selection.sent_at) >= SELECTION_EXPIRY
        {
            self.http_selection = None;
            return None;
        }
        let (after, prior) = selection.accepted.as_ref()?;
        let (clock, at) = self.clock.as_ref()?;
        let media = clock.media_id.as_ref()?;
        if let Some(confirmed) = &selection.confirmed_media
            && (!clock.active
                || confirmed != media
                || !selection.target.matches(clock)
                || (selection.prior_context_media.as_ref() == Some(confirmed)
                    && selection.inactive_restart.is_none()))
        {
            self.http_selection = None;
            return None;
        }
        if state.connection != ProviderConnectionState::Connected
            || state.authentication_required
            || !clock.active
            || clock.generation != selection.generation
            || clock.device_id != selection.device
            || clock.control_uptime_ms.is_none()
            || now.saturating_duration_since(*at) > CONTROL_FRESHNESS
            || clock.sequence <= *after
            || clock.metadata.is_none()
            || !selection.target.matches(clock)
            || (selection.confirmed_media.is_none()
                && (clock.state != PlaybackState::Playing
                    || clock.rate <= 0.0
                    || prior.as_ref() == Some(media)
                    // The runtime can announce the new context before it
                    // changes the media. A matching HTTP frame still binds.
                    || (selection.prior_context_media.as_ref() == Some(media)
                        && !selection
                            .inactive_restart
                            .as_ref()
                            .is_some_and(|restart| restart.confirms(clock, *at)))
                    || u128::from(clock.position_ms)
                        > now.saturating_duration_since(selection.sent_at).as_millis() + 4000))
        {
            return None;
        }
        if state
            .playback
            .as_ref()
            .and_then(|frame| frame.device.as_ref())
            .is_some_and(|device| {
                device.id == selection.device && device.is_restricted
                    || positioned_row
                        && device.id != selection.device
                        && device.is_active
                        && !device.is_restricted
            })
        {
            self.http_selection = None;
            return None;
        }
        if state.playback.as_ref().is_some_and(|frame| {
            frame
                .device
                .as_ref()
                .is_some_and(|device| device.id == selection.device && !device.is_restricted)
                && frame
                    .current
                    .as_ref()
                    .is_some_and(|item| super::mapping::same_playback_id(&item.id, media))
                && (!positioned_row
                    || frame.context_uri.as_ref()
                        == clock
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.context_uri.as_ref()))
        }) {
            // The ordinary exact HTTP/local match now owns the projection.
            self.http_selection = None;
            return None;
        }
        let device = state
            .devices
            .iter()
            .find(|device| device.id == selection.device && !device.is_restricted)?
            .clone();
        // The initial inactive same-media restart has stronger causal evidence
        // than a display-only row: accepted exact HTTP operation, full new-epoch
        // Loading/rewind, then full Playing. Capture is a separate sticky scope;
        // never populate the control binding or control_media from it.
        if !selection.capture_promoted
            && self.capture_binding.is_none()
            && matches!(selection.target, SelectionTarget::Context(_))
            && selection
                .inactive_restart
                .as_ref()
                .is_some_and(|restart| restart.confirms(clock, *at))
            && capture_device_is_current(state, &selection.device)
            && let (Some(media), Some(epoch), Some(uptime), Some(context)) = (
                clock.media_id.as_ref(),
                clock.queue_epoch.filter(|epoch| *epoch > 0),
                clock.control_uptime_ms,
                clock
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.context_uri.as_ref()),
            )
            && let Some(serial) = self.capture_serial.checked_add(1)
        {
            self.capture_serial = serial;
            selection.capture_promoted = true;
            self.capture_binding = Some(CaptureBinding {
                witness: OwnedCaptureWitness {
                    id: serial,
                    generation: clock.generation,
                    device_id: selection.device.clone(),
                    media_id: media.clone(),
                    queue_epoch: epoch,
                    context_uri: context.clone(),
                    playing_sequence: clock.sequence,
                    playing_control_uptime_ms: uptime,
                },
                operation: selection.operation,
                metadata: clock
                    .metadata
                    .as_ref()
                    .expect("validated full restart metadata")
                    .clone(),
                device: device.clone(),
            });
            self.inert_owned_destination = Some(selection.device.clone());
        }
        selection.confirmed_media = Some(media.clone());
        Some(device)
    }
    pub fn remember_catalog(&mut self, page: &RemotePage) {
        if !page.detail || page.issue.is_some() {
            return;
        }
        for chapter in page
            .items
            .iter()
            .filter(|item| item.id.as_str().starts_with("spotify:chapter:"))
        {
            if self
                .selected_chapter
                .as_ref()
                .is_some_and(|known| super::mapping::same_playback_id(&known.id, &chapter.id))
            {
                self.selected_chapter = Some(chapter.clone());
                self.dirty = true;
            }
            if self
                .chapter
                .as_ref()
                .is_some_and(|known| super::mapping::same_playback_id(&known.id, &chapter.id))
                || self
                    .known
                    .as_ref()
                    .and_then(|frame| frame.current.as_ref())
                    .is_some_and(|known| super::mapping::same_playback_id(&known.id, &chapter.id))
            {
                self.chapter = Some(chapter.clone());
                self.dirty = true;
            }
        }
    }
    pub fn has_owned_destination_veto(&self) -> bool {
        self.inert_owned_destination.is_some()
    }
    fn advance(&mut self, now: Instant) {
        self.serial = self.serial.wrapping_add(1);
        self.anchor_at = now;
    }
    pub fn set_rate_limit(&mut self, limit: Option<RateLimit>) {
        if self.rate_limit == limit {
            return;
        }
        self.rate_limit = limit;
        if let Some(base) = self.base.as_mut() {
            apply_rate(base, limit);
        }
        self.dirty = true;
    }
    pub fn set_local_control_result(&mut self, result: Option<RemoteControlResult>) {
        self.local_control_result = result;
        self.dirty = true;
    }
    pub fn republish(&mut self) {
        self.dirty = true;
    }
    pub fn ingest(&mut self, mut state: RemoteState, now: Instant) {
        // An already in-flight successful read cannot remove a newly observed
        // application quota. After expiry, require a fresh read to reconnect.
        apply_rate(&mut state, self.rate_limit);
        if let Some(owned) = &self.inert_owned_destination
            && self.raw_observation != Some(state.observation_id)
            && !state.authentication_required
            && state.connection == ProviderConnectionState::Connected
            && let Some(frame) = &state.playback
            && frame.current.is_some()
            && let Some(device) = &frame.device
            && device.id != *owned
            && super::valid_device_id(&device.id)
            && device.is_active
            && !device.is_restricted
            && !state
                .devices
                .iter()
                .any(|known| known.is_active && (known.id != device.id || known.is_restricted))
        {
            // Only a fresh playback read proves an external takeover. Device
            // merges and cached HTTP frames retain their raw observation ID.
            self.inert_owned_destination = None;
        }
        if self.capture_binding.as_ref().is_some_and(|binding| {
            retained_capture_device(&state, &binding.device).is_none()
                || self.clock.as_ref().is_none_or(|(clock, at)| {
                    now.saturating_duration_since(*at) > CONTROL_FRESHNESS
                        || !binding.matches_clock(clock)
                })
                || state.control_result.as_ref().is_some_and(|result| {
                    result.operation_id == binding.operation && result.error.is_some()
                })
        }) {
            self.capture_binding = None;
        }
        if state.authentication_required
            || state.connection == ProviderConnectionState::Disconnected
        {
            self.inert_owned_destination = None;
            self.clock = None;
            self.known = None;
            self.chapter = None;
            self.selected_chapter = None;
            self.clear_binding();
            self.queue = None;
            self.local_control_result = None;
            self.http_selection = None;
            self.context_resume = None;
        } else if let Some((_, bound)) = &self.binding
            && state.playback.as_ref().is_some_and(|frame| {
                frame
                    .device
                    .as_ref()
                    .is_none_or(|device| device.id != *bound || device.is_restricted)
            })
        {
            self.clear_binding();
            self.queue = None;
        }
        self.observe_selection_result(&state);
        if self.context_resume.as_ref().is_some_and(|resume| {
            state.control_result.as_ref().is_some_and(|result| {
                result.operation_id == resume.operation
                    && (result.error.is_some() || resume.dispatched_at.is_none())
            })
        }) {
            self.context_resume = None;
        }
        if let Some(frame) = &state.playback {
            self.known = Some(frame.clone());
        }
        if self.raw_observation != Some(state.observation_id) {
            self.raw_observation = Some(state.observation_id);
            self.base_at = now;
            self.advance(now);
        }
        self.base = Some(state);
        self.dirty = true;
    }
    /// Returns whether one coalesced metadata refresh is needed for a new binding.
    pub fn observe(
        &mut self,
        clock: Option<RemoteOutputClock>,
        now: Instant,
    ) -> Result<bool, PlayerError> {
        let Some(mut clock) = clock else {
            self.capture_binding = None;
            self.clock = None;
            self.pending_clock_at = None;
            self.refresh_key = None;
            self.clear_binding();
            self.queue = None;
            self.http_selection = None;
            self.context_resume = None;
            self.dirty = true;
            return Ok(false);
        };
        if clock.generation == 0
            || clock.sequence == 0
            || !super::valid_device_id(&clock.device_id)
            || clock.position_ms > 86_400_000
            || !clock.rate.is_finite()
            || !(0.0..=4.0).contains(&clock.rate)
            || !matches!(
                clock.state,
                PlaybackState::Idle
                    | PlaybackState::Loading
                    | PlaybackState::Playing
                    | PlaybackState::Paused
            )
            || clock.media_id.as_ref().is_some_and(|id| !playable_id(id))
            || (clock.media_id.is_none() && clock.state != PlaybackState::Idle)
            || clock
                .volume
                .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
            || clock.actions.as_ref().is_some_and(|a| {
                [a.seek_forward_ms, a.seek_backward_ms]
                    .into_iter()
                    .flatten()
                    .any(|ms| ms == 0 || ms > 300_000)
            })
        {
            return Err(PlayerError::invalid("Invalid owned-output observation"));
        }
        if (clock.generation, clock.sequence) <= self.watermark {
            return Ok(false);
        }
        self.watermark = (clock.generation, clock.sequence);
        if self.capture_binding.as_ref().is_some_and(|binding| {
            !binding.matches_clock(&clock)
                || self
                    .clock
                    .as_ref()
                    .is_none_or(|(_, at)| now.saturating_duration_since(*at) > CONTROL_FRESHNESS)
        }) {
            self.capture_binding = None;
        }
        if self.http_selection.as_ref().is_some_and(|selection| {
            clock.generation != selection.generation || clock.device_id != selection.device
        }) {
            self.http_selection = None;
        }
        let expired = self
            .clock
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) > EXPIRY);
        if expired
            || !clock.active
            || self.binding.as_ref().is_some_and(|(generation, device)| {
                *generation != clock.generation || *device != clock.device_id
            })
        {
            self.clear_binding();
        }
        if clock.state != PlaybackState::Loading && clock.actions.is_none() {
            self.navigation_actions = None;
        }
        if self
            .queue
            .as_ref()
            .is_some_and(|(queue, _)| !queue_matches(queue, &clock))
        {
            self.queue = None;
        }
        if clock.metadata.as_ref().is_some_and(|value| {
            clock.media_id.is_none()
                || clock.state == PlaybackState::Idle
                || !valid_metadata(value, 1)
        }) {
            clock.metadata = None;
        }
        if let Some(selection) = &mut self.http_selection
            && selection.inactive_restart.as_mut().is_some_and(|restart| {
                now < selection.sent_at
                    || !restart.observe(
                        &clock,
                        &selection.target,
                        now,
                        selection.confirmed_media.is_some(),
                    )
            })
        {
            selection.inactive_restart = None;
        }
        if self
            .context_resume
            .as_mut()
            .is_some_and(|resume| !resume.observe(&clock, now))
        {
            self.context_resume = None;
        }
        if (clock.state != PlaybackState::Paused || clock.rate != 0.0)
            && let Some(dispatch) = self
                .resume_pause
                .as_mut()
                .and_then(|pause| pause.dispatch.as_mut())
        {
            dispatch.stationary_position = None;
        }
        let key = (
            clock.generation,
            clock.device_id.clone(),
            clock.media_id.clone(),
            clock.active,
            clock.metadata.is_some(),
        );
        let base_frame = self.base.as_ref().and_then(|s| s.playback.as_ref());
        let needs_refresh = if clock.active {
            !base_frame.is_some_and(|p| {
                p.device.as_ref().is_some_and(|d| d.id == clock.device_id)
                    && p.current
                        .as_ref()
                        .zip(clock.media_id.as_ref())
                        .is_some_and(|(item, id)| super::mapping::same_playback_id(&item.id, id))
            })
        } else {
            base_frame.or(self.known.as_ref()).is_some_and(|p| {
                p.device
                    .as_ref()
                    .is_some_and(|d| d.id == clock.device_id && d.is_active)
            })
        };
        let refresh = self.refresh_key.as_ref() != Some(&key) && needs_refresh;
        self.refresh_key = Some(key);
        self.clock = Some((clock, now));
        self.pending_clock_at = Some(now);
        self.dirty = true;
        Ok(refresh)
    }
    pub fn observe_queue(
        &mut self,
        mut queue: RemoteOutputQueue,
        now: Instant,
    ) -> Result<(), PlayerError> {
        if queue.sequence == 0
            || queue.epoch == 0
            || queue.previous.len() > 10
            || queue.upcoming.len() > 10
            || !super::valid_device_id(&queue.device_id)
            || !playable_id(&queue.media_id)
        {
            return Err(PlayerError::invalid("Invalid owned queue observation"));
        }
        if !self.clock.as_ref().is_some_and(|(clock, at)| {
            queue_matches(&queue, clock) && now.saturating_duration_since(*at) <= EXPIRY
        }) || (queue.generation, queue.sequence) <= self.queue_watermark
        {
            return Ok(());
        }
        for entries in [&mut queue.previous, &mut queue.upcoming] {
            entries.retain_mut(|entry| {
                if entry
                    .metadata
                    .as_ref()
                    .is_some_and(|value| !valid_metadata(value, 0))
                {
                    entry.metadata = None;
                }
                playable_id(&entry.media_id)
            });
        }
        self.queue_watermark = (queue.generation, queue.sequence);
        self.queue = Some((queue, now));
        self.dirty = true;
        Ok(())
    }
    pub fn queue_has_titles(&self) -> bool {
        self.queue
            .as_ref()
            .is_some_and(|(queue, _)| queue.upcoming.iter().all(|entry| entry.metadata.is_some()))
    }
    pub fn autoplay_queue(&self, now: Instant) -> Option<(&RemoteOutputQueue, Duration)> {
        let clock = self.navigation_clock(now)?;
        let (queue, at) = self.queue.as_ref()?;
        queue_matches(queue, clock).then_some((queue, now.saturating_duration_since(*at)))
    }
    pub(super) fn previous_witness(
        &self,
        now: Instant,
    ) -> Option<super::owned_http::PreviousWitness> {
        self.previous_witness_within(now, CONTROL_FRESHNESS, false)
    }
    pub(super) fn cued_previous_witness(
        &self,
        now: Instant,
    ) -> Option<super::owned_http::PreviousWitness> {
        self.previous_witness_within(now, QUEUE_EXPIRY, true)
    }
    pub(super) fn next_context_head(&self, now: Instant) -> Option<&MediaId> {
        let (queue, age) = self.autoplay_queue(now)?;
        let next = queue.upcoming.first()?;
        (age <= CONTROL_FRESHNESS
            && next.source == Some(QueueEntrySource::Context)
            && !super::mapping::same_playback_id(&next.media_id, &queue.media_id))
        .then_some(&next.media_id)
    }
    fn previous_witness_within(
        &self,
        now: Instant,
        limit: Duration,
        context_only: bool,
    ) -> Option<super::owned_http::PreviousWitness> {
        let clock = self.control_clock(now)?;
        let (queue, age) = self.autoplay_queue(now)?;
        if age > limit {
            return None;
        }
        let head = queue.previous.first()?;
        if context_only && head.source != Some(QueueEntrySource::Context) {
            return None;
        }
        let predecessor = &head.media_id;
        if super::mapping::same_playback_id(predecessor, clock.media_id.as_ref()?) {
            return None;
        }
        Some(super::owned_http::PreviousWitness {
            predecessor: predecessor.clone(),
            context: clock.metadata.as_ref()?.context_uri.clone()?,
            queue_epoch: queue.epoch,
            observed_at: now - age,
            unambiguous_context_predecessor: head.source == Some(QueueEntrySource::Context)
                && queue
                    .previous
                    .iter()
                    .skip(1)
                    .chain(&queue.upcoming)
                    .all(|row| !super::mapping::same_playback_id(&row.media_id, predecessor)),
        })
    }
    pub fn take(&mut self, now: Instant) -> Option<RemoteState> {
        if self.control_media.is_some() && self.control_clock(now).is_none() {
            self.dirty = true;
        }
        if self
            .clock
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) > EXPIRY)
        {
            self.clock = None;
            self.refresh_key = None;
            self.clear_binding();
            self.queue = None;
            self.dirty = true;
        }
        if self
            .queue
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) > QUEUE_EXPIRY)
        {
            self.queue = None;
            self.dirty = true;
        }
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        let mut state = self.base.clone()?;
        if let Some(result) = &self.local_control_result {
            state.control_result = Some(result.clone());
        }
        self.control_media = None;
        let (witnessed_device, pause_observation) = match self.witnessed_resume_pause(&state, now) {
            Some((device, observation)) => (Some(device), Some(observation)),
            None => (
                self.witnessed_context_resume(&state, now)
                    .or_else(|| self.selection_device(&state, now))
                    .or_else(|| self.capture_device(&state, now)),
                None,
            ),
        };
        let provisional_pause = matches!(
            pause_observation,
            Some(ResumePauseObservation::Provisional(_))
        );
        let displayed_pause_position = match pause_observation {
            Some(
                ResumePauseObservation::Provisional(position_ms)
                | ResumePauseObservation::Retained(position_ms),
            ) => Some(position_ms),
            _ => None,
        };
        let pending_clock_at = self.pending_clock_at.take();
        let witnessed = witnessed_device.is_some();
        let connected = state.connection == ProviderConnectionState::Connected
            && !state.authentication_required;
        let bound = |c: &RemoteOutputClock| {
            self.binding.as_ref().is_some_and(|(generation, device)| {
                *generation == c.generation && *device == c.device_id
            })
        };
        let clock = self.clock.as_ref().filter(|(c, _)| {
            c.active
                && (connected
                    || (!state.authentication_required
                        && state.connection == ProviderConnectionState::Unavailable
                        && (bound(c)
                            || self.capture_binding.as_ref().is_some_and(|binding| {
                                retained_capture_device(&state, &binding.device).is_some()
                                    && binding.matches_clock(c)
                            }))))
        });
        let http_frame = state.playback.is_some();
        let mut frame = state.playback.take();
        if let Some(device) = witnessed_device {
            // The exact requested device and the freshly witnessed selection
            // may be ahead of the cloud poll. Do not inherit its old queue.
            frame = Some(RemotePlayback {
                enhancement: None,
                context_uri: None,
                state: PlaybackState::Loading,
                position_ms: 0,
                position_rate: 0.0,
                volume: device.volume.unwrap_or_default(),
                current: None,
                upcoming: vec![],
                device_queue: None,
                shuffle: false,
                repeat: RepeatMode::Off,
                capabilities: Capabilities::default(),
                transport: TransportControls::default(),
                device: Some(device),
            });
            if let Some(binding) = self.capture_binding.as_ref()
                && clock.is_some_and(|(clock, at)| {
                    now.saturating_duration_since(*at) <= CONTROL_FRESHNESS
                        && binding.matches_clock(clock)
                })
                && let Some(frame) = &mut frame
            {
                // Exact-scope static metadata can survive a partial paused
                // heartbeat. Neither the raw clock nor its timestamp changes.
                let metadata = clock
                    .and_then(|(clock, _)| clock.metadata.as_ref())
                    .unwrap_or(&binding.metadata);
                use_owned_metadata(frame, &binding.witness.media_id, metadata);
            }
        }
        // A positive observation can bridge a transient HTTP 204. The previous
        // device is required even when the runtime already knows the new item.
        if frame.is_none() && clock.is_some() {
            frame = self.known.clone();
        }
        if let Some(frame) = &mut frame {
            let accepted = clock.filter(|(c, _)| {
                if !frame
                    .device
                    .as_ref()
                    .is_some_and(|d| d.id == c.device_id && !d.is_restricted)
                {
                    return false;
                }
                if !witnessed
                    && let Some(selection) = self.http_selection.as_ref()
                    && let SelectionTarget::ContextRow { uri, item } = &selection.target
                    && frame.context_uri.as_ref() != Some(uri)
                    && c.media_id.as_ref() == Some(item)
                {
                    // A bound runtime can already have the row while the HTTP
                    // frame still describes another context. Wait for the
                    // exact row witness or normal HTTP reconciliation.
                    return false;
                }
                if frame
                    .current
                    .as_ref()
                    .zip(c.media_id.as_ref())
                    .is_some_and(|(item, id)| super::mapping::same_playback_id(&item.id, id))
                {
                    if bound(c)
                        && self.http_selection.as_ref().is_none_or(|selection| {
                            !matches!(selection.target, SelectionTarget::ContextRow { .. })
                                || selection.target.matches(c)
                        })
                        && let Some(metadata) = &c.metadata
                    {
                        frame.context_uri.clone_from(&metadata.context_uri);
                    }
                    return true;
                }
                if let (true, Some(id), Some(metadata)) =
                    (witnessed || bound(c), &c.media_id, &c.metadata)
                {
                    use_owned_metadata(frame, id, metadata);
                    return true;
                }
                false
            });
            if frame
                .current
                .as_ref()
                .zip(self.selected_chapter.as_ref())
                .is_some_and(|(current, selected)| {
                    super::mapping::same_playback_id(&current.id, &selected.id)
                })
            {
                self.chapter = self.selected_chapter.take();
            }
            apply_chapter_metadata(frame, self.chapter.as_ref());
            if accepted.is_some()
                && displayed_pause_position.is_none()
                && let Some(at) = pending_clock_at
            {
                self.serial = self.serial.wrapping_add(1);
                self.anchor_at = at;
            }
            let (position, rate, at) = if let Some((clock, at)) = accepted {
                if connected && http_frame && !witnessed && clock.media_id.is_some() {
                    self.binding = Some((clock.generation, clock.device_id.clone()));
                }
                state.owned_output_observed = !provisional_pause;
                if !provisional_pause && (witnessed || self.binding.is_some()) {
                    if let Some((queue, _)) = self
                        .queue
                        .as_ref()
                        .filter(|(queue, _)| queue_matches(queue, clock))
                    {
                        project_queue(frame, self.known.as_ref(), queue);
                    } else if clock.queue_epoch.is_some()
                        && (!connected
                            || clock
                                .metadata
                                .as_ref()
                                .is_some_and(|m| m.context_uri != frame.context_uri))
                    {
                        frame.upcoming.clear();
                    }
                }
                if self.binding.is_some()
                    && !witnessed
                    && clock.control_uptime_ms.is_some()
                    && now.saturating_duration_since(*at) <= CONTROL_FRESHNESS
                {
                    self.control_media.clone_from(&clock.media_id);
                    state.owned_output_control = self.control_media.is_some();
                    if state.owned_output_control {
                        self.inert_owned_destination = Some(clock.device_id.clone());
                    }
                }
                frame.state = if provisional_pause {
                    PlaybackState::Loading
                } else {
                    clock.state
                };
                if !connected && state.owned_output_control {
                    // During HTTP failure expose only the actions evidenced by Soloist.
                    frame.capabilities = Capabilities::default();
                    frame.transport = TransportControls::default();
                    frame.capabilities.volume = clock.volume.is_some();
                }
                if let Some(device) = frame.device.as_mut() {
                    device.is_active = true;
                }
                if let Some(volume) = clock.volume {
                    frame.volume = volume;
                    if let Some(device) = frame.device.as_mut() {
                        device.volume = Some(volume);
                    }
                    for device in &mut state.devices {
                        if device.id == clock.device_id {
                            device.volume = Some(volume);
                        }
                    }
                }
                if let Some(options) = &clock.options {
                    frame.shuffle = options.shuffle;
                    frame.repeat = options.repeat;
                    frame.enhancement = options.enhancement;
                }
                if !witnessed && let Some(actions) = &clock.actions {
                    self.navigation_actions =
                        state
                            .owned_output_control
                            .then_some((actions.next, actions.previous, *at));
                    let item = frame
                        .current
                        .as_ref()
                        .map_or(Capabilities::default(), |i| i.capabilities);
                    frame.capabilities.play = item.play && actions.play;
                    frame.capabilities.pause = item.pause && actions.pause;
                    frame.capabilities.seek = item.seek && actions.seek;
                    frame.transport.next = actions.next;
                    frame.transport.previous = actions.previous;
                    frame.transport.shuffle = actions.shuffle;
                    frame.transport.repeat = actions.repeat;
                    frame.transport.queue_add =
                        actions.queue_add && frame.device.as_ref().is_some_and(|d| d.is_active);
                    frame.transport.seek_forward_ms =
                        actions.seek_forward_ms.filter(|_| frame.capabilities.seek);
                    frame.transport.seek_backward_ms =
                        actions.seek_backward_ms.filter(|_| frame.capabilities.seek);
                } else if state.owned_output_control && clock.state == PlaybackState::Loading {
                    // Keep only recent relative controls on the same owned output.
                    // The queued command still waits for the new item's actions.
                    let (next, previous) = self
                        .navigation_actions
                        .filter(|(_, _, observed)| {
                            now.saturating_duration_since(*observed) <= CONTROL_FRESHNESS
                        })
                        .map_or((false, false), |(next, previous, _)| (next, previous));
                    frame.transport.next = next;
                    frame.transport.previous = previous;
                }
                (
                    displayed_pause_position.unwrap_or(clock.position_ms),
                    clock.rate,
                    *at,
                )
            } else {
                (frame.position_ms, frame.position_rate, self.base_at)
            };
            let elapsed = if (connected || state.owned_output_observed)
                && frame.state == PlaybackState::Playing
            {
                self.anchor_at.saturating_duration_since(at).as_secs_f64() * 1000.0 * rate
            } else {
                0.0
            };
            frame.position_ms = position
                .saturating_add(elapsed as u64)
                .min(frame.current.as_ref().map_or(0, |i| i.duration_ms));
            frame.position_rate = rate;
            if !http_frame && accepted.is_none() {
                frame.current = None;
            }
        }
        if !http_frame && !frame.as_ref().is_some_and(|p| p.current.is_some()) {
            frame = None;
        }
        state.playback = frame;
        state.observation_id = self.serial;
        Some(state)
    }
}

fn apply_chapter_metadata(frame: &mut RemotePlayback, chapter: Option<&MediaItem>) {
    let Some((current, chapter)) = frame.current.as_mut().zip(chapter) else {
        return;
    };
    // Playback exposes a book as an Episode/Show without authors. One retained
    // chapter supplies display identity only; the observed transport stays authoritative.
    if !current.id.as_str().starts_with("spotify:episode:")
        || !super::mapping::same_playback_id(&current.id, &chapter.id)
    {
        return;
    }
    current.id.clone_from(&chapter.id);
    current.artists.clone_from(&chapter.artists);
    current.album.clone_from(&chapter.album);
    current.location.clone_from(&chapter.location);
    if current.artwork.is_none() {
        current.artwork.clone_from(&chapter.artwork);
    }
    current.capabilities.library_save = false;
    current.capabilities.play &= chapter.capabilities.play;
    frame.capabilities.play &= current.capabilities.play;
}

fn playable_id(id: &MediaId) -> bool {
    ["spotify:track:", "spotify:episode:", "spotify:chapter:"]
        .iter()
        .any(|prefix| {
            id.as_str()
                .strip_prefix(prefix)
                .is_some_and(super::mapping::valid_id)
        })
}

fn valid_metadata(value: &RemoteOutputMetadata, minimum_duration: u64) -> bool {
    let text =
        |s: &str, max| !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control);
    text(&value.title, 384)
        && value.artists.len() <= 6
        && value.artists.iter().all(|s| text(s, 128))
        && value.album.as_deref().is_none_or(|s| text(s, 256))
        && (minimum_duration..=86_400_000).contains(&value.duration_ms)
        && value.artwork.as_deref().is_none_or(|s| {
            s.len() <= 512
                && !s.contains(['?', '#', '\\'])
                && !s.chars().any(char::is_control)
                && super::mapping::valid_image_url(s)
        })
        && value
            .context_uri
            .as_deref()
            .is_none_or(super::mapping::valid_uri)
}

fn queue_matches(queue: &RemoteOutputQueue, clock: &RemoteOutputClock) -> bool {
    clock.active
        && clock.generation == queue.generation
        && clock.device_id == queue.device_id
        && clock.media_id.as_ref() == Some(&queue.media_id)
        && clock.queue_epoch == Some(queue.epoch)
}

fn project_queue(
    frame: &mut RemotePlayback,
    known: Option<&RemotePlayback>,
    queue: &RemoteOutputQueue,
) {
    let row = |entry: &RemoteOutputQueueEntry| {
        let matching = known
            .into_iter()
            .flat_map(|p| p.current.iter().chain(p.upcoming.iter()))
            .find(|item| super::mapping::same_playback_id(&item.id, &entry.media_id));
        matching
            .cloned()
            .unwrap_or_else(|| owned_item(&entry.media_id, entry.metadata.as_ref()))
    };
    frame.upcoming = queue.upcoming.iter().map(row).collect();
    frame.device_queue = Some(DeviceQueueDetails {
        previous: queue.previous.iter().map(row).collect(),
        previous_sources: queue.previous.iter().map(|entry| entry.source).collect(),
        upcoming_sources: queue.upcoming.iter().map(|entry| entry.source).collect(),
    });
}

fn owned_item(id: &MediaId, value: Option<&RemoteOutputMetadata>) -> MediaItem {
    let path = id
        .as_str()
        .strip_prefix("spotify:")
        .expect("validated owned URI")
        .replace(':', "/");
    let mut capabilities = value.map_or(Capabilities::default(), |_| Capabilities::REMOTE);
    capabilities.library_save = value.is_some() && !path.starts_with("chapter/");
    MediaItem {
        id: id.clone(),
        title: value.map_or_else(|| "Track details unavailable".into(), |m| m.title.clone()),
        artists: value.map_or_else(Vec::new, |m| m.artists.clone()),
        album: value.and_then(|m| m.album.clone()),
        duration_ms: value.map_or(0, |m| m.duration_ms),
        artwork: value.and_then(|m| m.artwork.clone()),
        location: Some(MediaLocation::Remote {
            uri: id.as_str().to_owned(),
            url: format!("https://open.spotify.com/{path}"),
            service: "Spotify".into(),
        }),
        capabilities,
    }
}

fn use_owned_metadata(frame: &mut RemotePlayback, id: &MediaId, value: &RemoteOutputMetadata) {
    frame.current = Some(owned_item(id, Some(value)));
    frame.context_uri.clone_from(&value.context_uri);
    frame.upcoming.clear();
    frame.capabilities = Capabilities {
        volume: frame.capabilities.volume,
        ..Default::default()
    };
    frame.transport = TransportControls::default();
    frame.shuffle = false;
    frame.repeat = RepeatMode::Off;
    frame.enhancement = None;
}

fn apply_rate(state: &mut RemoteState, limit: Option<RateLimit>) {
    if !state.authentication_required
        && state.connection != ProviderConnectionState::Disconnected
        && let Some(limit) = limit
    {
        state.connection = ProviderConnectionState::Unavailable;
        state.retry_after_seconds = Some(limit.seconds);
        state.issue = Some(limit.issue());
    }
}
