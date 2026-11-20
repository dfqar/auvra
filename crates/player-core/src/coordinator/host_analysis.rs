use super::*;
use player_domain::{OwnedCaptureWitness, RemoteOutputClock, RemoteOutputQueue};
use std::time::Instant;

const LEASE_FRESHNESS: Duration = Duration::from_millis(800);
const SPATIAL_RESUME_LIMIT: Duration = Duration::from_millis(1700);
const SPATIAL_NEXT_LIMIT: Duration = Duration::from_millis(1250);
const MANUAL_SPATIAL_REQUEST_LIMIT: usize = 8;
const NATURAL_SPATIAL_TAIL_MS: u64 = 1_000;
// Outgoing duration/position metadata can differ by a small timestamp skew.
// Only a different, exactly observed native queue head may use this allowance.
const SUCCESSOR_FIRST_POSITION_SKEW_MS: u64 = 250;
// The host refreshes an unchanged native queue every twelve seconds and expires it at eighteen.
const SPATIAL_QUEUE_FRESHNESS: Duration = Duration::from_secs(18);

struct DeferredSpatialPlay {
    lease: HostAnalysisLease,
    deadline: Instant,
    request_id: Option<u64>,
}

struct SpatialResumeClockGap {
    lease: HostAnalysisLease,
    paused: SpatialOutputClock,
    deadline: Instant,
}

#[derive(Clone)]
struct SpatialOutputClock {
    provider: String,
    device: String,
    media: Option<MediaId>,
    context: Option<String>,
    generation: u64,
    sequence: u64,
    queue_epoch: Option<u64>,
    control_uptime_ms: Option<u64>,
    position_ms: u64,
    rate: f64,
    observed: Instant,
    full: bool,
    metadata_present: bool,
    stationary_pause: bool,
    moving_playback: bool,
    loading: bool,
    incomplete_zero_playback: bool,
}

impl SpatialOutputClock {
    fn new(provider: &str, clock: &RemoteOutputClock) -> Self {
        Self {
            provider: provider.into(),
            device: clock.device_id.clone(),
            media: clock.media_id.clone(),
            context: clock
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.context_uri.clone()),
            generation: clock.generation,
            sequence: clock.sequence,
            queue_epoch: clock.queue_epoch,
            control_uptime_ms: clock.control_uptime_ms,
            position_ms: clock.position_ms,
            rate: clock.rate,
            observed: Instant::now(),
            full: clock.actions.is_some(),
            metadata_present: clock.metadata.is_some(),
            stationary_pause: Self::stationary_pause(clock),
            moving_playback: clock.active
                && clock.control_uptime_ms.is_some()
                && clock.state == PlaybackState::Playing
                && (0.0..=4.0).contains(&clock.rate)
                && clock.rate > 0.0,
            loading: clock.active
                && clock.control_uptime_ms.is_some()
                && clock.state == PlaybackState::Loading
                && clock.rate == 0.0,
            incomplete_zero_playback: clock.active
                && clock.control_uptime_ms.is_some()
                && clock.state == PlaybackState::Playing
                && clock.rate == 0.0
                && clock.actions.is_none(),
        }
    }

    fn stationary_pause(clock: &RemoteOutputClock) -> bool {
        clock.active
            && clock.control_uptime_ms.is_some()
            && clock.state == PlaybackState::Paused
            && clock.rate == 0.0
    }

    fn matches_lease(&self, lease: &HostAnalysisLease) -> bool {
        self.provider == lease.provider
            && self.device == lease.device_id
            && self.media.as_ref() == Some(&lease.media_id)
    }

    fn projected_tail_position(
        &self,
        incoming: &RemoteOutputClock,
        now: Instant,
    ) -> Option<TailProjection> {
        let observed_age = now.saturating_duration_since(self.observed);
        if !self.full
            || !self.moving_playback
            || incoming.generation != self.generation
            || observed_age > LEASE_FRESHNESS
            || !self.rate.is_finite()
            || self.rate <= 0.0
            || self.rate > 4.0
        {
            return None;
        }
        let control_delta_ms = incoming
            .control_uptime_ms?
            .checked_sub(self.control_uptime_ms?)?;
        if Duration::from_millis(control_delta_ms) > LEASE_FRESHNESS {
            return None;
        }
        // The host may already have extrapolated the old position up to its
        // forwarding time while retaining the original packet uptime. Never
        // add that delay twice, or advance on observation age alone.
        let applied_delta_ms = control_delta_ms.min(observed_age.as_millis() as u64);
        let advance = (applied_delta_ms as f64 * self.rate).floor() as u64;
        Some(TailProjection {
            position_ms: self.position_ms.checked_add(advance)?,
            control_delta_ms,
            applied_delta_ms,
        })
    }
}

#[derive(Clone, Copy)]
struct TailProjection {
    position_ms: u64,
    control_delta_ms: u64,
    applied_delta_ms: u64,
}

#[derive(PartialEq, Eq)]
struct SuccessorRejectScope {
    provider: String,
    lease_generation: u64,
    runtime_generation: u64,
    successor: MediaId,
}

struct RendererOnlyPause {
    lease_generation: u64,
    runtime_generation: u64,
    queue_epoch: u64,
    playing_sequence: u64,
    playing_observed: Instant,
    confirmed: bool,
    authority: CaptureAuthority,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureAuthority {
    Control,
    Witness(u64),
}

impl RendererOnlyPause {
    fn matches(&self, lease: &HostAnalysisLease, clock: &SpatialOutputClock, epoch: u64) -> bool {
        self.lease_generation == lease.generation
            && self.runtime_generation == clock.generation
            && self.queue_epoch == epoch
            && clock.sequence >= self.playing_sequence
    }
}

struct WarmingSpatialNext {
    lease: HostAnalysisLease,
    deadline: Instant,
    paused_origin: Option<SpatialOutputClock>,
    natural: Option<NaturalSpatialNext>,
    left_paused_origin: bool,
    manual: Option<ManualSpatialNext>,
}

struct ManualSpatialNext {
    origin: SpatialOutputClock,
    requests: Vec<ManualSpatialRequest>,
    last_sequence: u64,
    last_uptime_ms: u64,
    last_epoch: u64,
    last_media: Option<MediaId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManualSpatialObservation {
    Playing,
    Dispatch,
    RetainedLoading,
}

struct ManualSpatialRequest {
    request_id: u64,
    previous: bool,
    dispatch: Option<ManualSpatialDispatch>,
}

struct ManualSpatialDispatch {
    id: u64,
    sequence: u64,
    epoch: u64,
    acknowledged: bool,
}

impl ManualSpatialRequest {
    fn matches_control(&self, control: &RemoteOutputControl) -> bool {
        if self.previous {
            matches!(
                control,
                RemoteOutputControl::Previous | RemoteOutputControl::PreviousFromStart
            )
        } else {
            *control == RemoteOutputControl::Next
        }
    }
}

impl ManualSpatialNext {
    fn retains_clock(&mut self, clock: &RemoteOutputClock) -> bool {
        let Some((uptime, epoch)) = clock
            .control_uptime_ms
            .zip(clock.queue_epoch.filter(|epoch| *epoch > 0))
        else {
            return false;
        };
        let incoming_context = clock
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.context_uri.as_ref());
        // track_changed has an identified item/epoch before the complete state
        // supplies context/actions. Keep only zero preroll through that marker.
        let contextless_loading = incoming_context.is_none()
            && self.origin.context.is_some()
            && clock.actions.is_none()
            && clock.metadata.is_some()
            && clock.state == PlaybackState::Loading
            && clock.rate == 0.0
            && clock
                .media_id
                .as_ref()
                .is_some_and(|media| Some(media) != self.origin.media.as_ref())
            && self.origin.queue_epoch.is_some_and(|origin| epoch > origin);
        let retain = clock.generation == self.origin.generation
            && clock.media_id.is_some()
            && clock.sequence >= self.last_sequence
            && uptime >= self.last_uptime_ms
            && epoch >= self.last_epoch
            && !(clock.actions.is_some() && clock.metadata.is_none())
            && (incoming_context == self.origin.context.as_ref() || contextless_loading);
        if retain {
            self.last_sequence = clock.sequence;
            self.last_uptime_ms = uptime;
            self.last_epoch = epoch;
            self.last_media.clone_from(&clock.media_id);
        }
        retain
    }
}

struct NaturalSpatialNext {
    origin: SpatialOutputClock,
    context: String,
    successor: Option<(MediaId, Option<u64>)>,
}

struct SpatialSuccessorQueue {
    provider: String,
    generation: u64,
    device: String,
    media: MediaId,
    epoch: u64,
    successor: MediaId,
    observed: Instant,
}

impl SpatialSuccessorQueue {
    fn confirms(&self, origin: &SpatialOutputClock, successor: &MediaId) -> bool {
        self.provider == origin.provider
            && self.generation == origin.generation
            && self.device == origin.device
            && Some(&self.media) == origin.media.as_ref()
            && Some(self.epoch) == origin.queue_epoch
            && &self.successor == successor
            && self.observed.elapsed() <= SPATIAL_QUEUE_FRESHNESS
    }
}

impl NaturalSpatialNext {
    fn observe(&mut self, clock: &RemoteOutputClock) -> bool {
        let Some((media, epoch)) = clock.media_id.as_ref().zip(clock.queue_epoch) else {
            return false;
        };
        let context = clock
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.context_uri.as_deref());
        if !clock.active
            || clock.control_uptime_ms.is_none()
            || clock.device_id != self.origin.device
            || clock.generation != self.origin.generation
            || clock.sequence <= self.origin.sequence
            || context.is_some_and(|context| context != self.context)
        {
            return false;
        }
        if Some(media) == self.origin.media.as_ref() {
            return self.successor.is_none()
                && Some(epoch) == self.origin.queue_epoch
                && clock.state == PlaybackState::Loading
                && clock.rate == 0.0;
        }
        if self
            .origin
            .queue_epoch
            .is_none_or(|previous| epoch <= previous)
            || clock.position_ms > NATURAL_SPATIAL_TAIL_MS
            || !matches!(clock.state, PlaybackState::Loading | PlaybackState::Playing)
        {
            return false;
        }
        let (expected, confirmed_epoch) =
            self.successor.get_or_insert_with(|| (media.clone(), None));
        if media != expected || confirmed_epoch.is_some_and(|confirmed| epoch != confirmed) {
            return false;
        }
        // The track marker can clear the context and advance the queue epoch.
        // A complete playing snapshot restores the context and may advance it again.
        if clock.state == PlaybackState::Playing
            && clock.actions.is_some()
            && clock.rate > 0.0
            && clock.rate <= 4.0
            && context == Some(self.context.as_str())
        {
            *confirmed_epoch = Some(epoch);
        }
        true
    }

    fn confirms(&self, lease: &HostAnalysisLease, clock: &SpatialOutputClock) -> bool {
        self.successor.as_ref().is_some_and(|(media, epoch)| {
            clock.matches_lease(lease)
                && &lease.media_id == media
                && epoch.is_some()
                && clock.full
                && clock.moving_playback
                && clock.sequence > self.origin.sequence
                && clock.generation == self.origin.generation
                && clock.queue_epoch == *epoch
                && clock.context.as_deref() == Some(self.context.as_str())
                && clock.observed.elapsed() <= LEASE_FRESHNESS
        })
    }

    fn matches_host_retirement(&self, clock: &SpatialOutputClock) -> bool {
        self.successor.as_ref().is_some_and(|(media, epoch)| {
            clock.provider == self.origin.provider
                && clock.device == self.origin.device
                && clock.generation == self.origin.generation
                && clock.sequence > self.origin.sequence
                && clock.media.as_ref() == Some(media)
                && clock
                    .queue_epoch
                    .zip(self.origin.queue_epoch)
                    .is_some_and(|(next, old)| next > old)
                && epoch.is_none_or(|epoch| Some(epoch) == clock.queue_epoch)
                && clock.observed.elapsed() <= LEASE_FRESHNESS
                && clock.position_ms <= NATURAL_SPATIAL_TAIL_MS
                && (clock.loading || clock.full && clock.moving_playback)
                && clock
                    .context
                    .as_ref()
                    .is_none_or(|context| context == &self.context)
        })
    }
}

struct WarmingSpatialSeek {
    lease: HostAnalysisLease,
    origin: SpatialOutputClock,
    request_id: u64,
    deadline: Instant,
}

impl WarmingSpatialSeek {
    fn matches_clock(&self, clock: &RemoteOutputClock) -> bool {
        clock.active
            && clock.control_uptime_ms.is_some()
            && clock.generation == self.origin.generation
            && clock.sequence >= self.origin.sequence
            && clock.queue_epoch == self.origin.queue_epoch
            && clock.media_id == self.origin.media
            && clock.device_id == self.origin.device
            && matches!(clock.state, PlaybackState::Playing | PlaybackState::Loading)
    }

    fn confirms(
        &self,
        lease: &HostAnalysisLease,
        clock: &SpatialOutputClock,
        request_id: u64,
        sequence: u64,
    ) -> bool {
        request_id == self.request_id
            && lease.playing
            && lease.spatial_stereo
            && lease.generation > self.lease.generation
            && lease.process_id == self.lease.process_id
            && self.origin.matches_lease(lease)
            && clock.matches_lease(lease)
            && clock.moving_playback
            && clock.generation == self.origin.generation
            && clock.queue_epoch == self.origin.queue_epoch
            && sequence == clock.sequence
            && sequence > self.origin.sequence
            && clock.observed.elapsed() <= LEASE_FRESHNESS
            && Instant::now() < self.deadline
    }
}

#[derive(Default)]
pub(super) struct HostAnalysis {
    pub(super) source: Option<Box<dyn HostOutputAnalysis>>,
    lease: Option<(HostAnalysisLease, Instant)>,
    pause_pending_until: Option<Instant>,
    deferred_play: Option<DeferredSpatialPlay>,
    resume_clock_gap: Option<SpatialResumeClockGap>,
    warming_next: Option<WarmingSpatialNext>,
    warming_seek: Option<WarmingSpatialSeek>,
    output_clock: Option<SpatialOutputClock>,
    applied_output_clock: Option<(u64, u64)>,
    leased_output_clock: Option<SpatialOutputClock>,
    successor_queue: Option<SpatialSuccessorQueue>,
    successor_queue_watermarks: std::collections::HashMap<String, (u64, u64)>,
    last_successor_reject: Option<SuccessorRejectScope>,
    renderer_only_pause: Option<RendererOnlyPause>,
    revoked: u64,
}
impl PlaybackCoordinator {
    pub fn register_host_analysis(&mut self, source: impl HostOutputAnalysis + 'static) {
        self.host_analysis.source = Some(Box::new(source));
    }
    pub(super) fn host_analysis_revoked_through(&self) -> Option<u64> {
        let revoked = self.host_analysis.revoked;
        (revoked != 0).then_some(revoked)
    }
    pub(super) fn clear_host_analysis(&mut self) {
        self.cancel_host_spatial_next();
        self.host_analysis.output_clock = None;
        self.host_analysis.applied_output_clock = None;
        self.host_analysis.leased_output_clock = None;
        self.host_analysis.successor_queue = None;
        self.host_analysis.renderer_only_pause = None;
        self.host_analysis.pause_pending_until = None;
        self.host_analysis.resume_clock_gap = None;
        if let Some((lease, _)) = self.host_analysis.lease.take() {
            self.host_analysis.revoked = self.host_analysis.revoked.max(lease.generation);
        }
        if let Some(source) = self.host_analysis.source.as_mut() {
            source.set_lease(None);
            source.set_confirmed_spatial_pause(None);
            source.set_confirmed_renderer_only_pause(None);
        }
    }
    pub(super) fn prepare_host_spatial_next(&mut self, request_id: Option<u64>) -> bool {
        let Some((lease, at)) = &self.host_analysis.lease else {
            self.trace_spatial_next("manual_no_lease", false);
            return false;
        };
        let paused_origin = self
            .host_analysis
            .output_clock
            .as_ref()
            .filter(|clock| {
                !lease.playing
                    && clock.stationary_pause
                    && clock.observed.elapsed() <= LEASE_FRESHNESS
                    && clock.matches_lease(lease)
                    && self.host_spatial_status() == Some(HostSpatialStatus::Paused)
                    && self.owned_playback(lease).is_some_and(|playback| {
                        playback.state == PlaybackState::Paused && playback.position_rate == 0.0
                    })
            })
            .cloned();
        if !lease.spatial_stereo
            || at.elapsed() > LEASE_FRESHNESS
            || !(self.host_analysis_is_playing(lease) || paused_origin.is_some())
            || !self
                .remote
                .states
                .get(&lease.provider)
                .is_some_and(|state| state.owned_output_control && state.transport_available())
            || !self.owned_playback(lease).is_some_and(|p| p.transport.next)
        {
            self.trace_spatial_next("manual_rejected", false);
            return false;
        }
        self.trace_spatial_next("manual_admitted", self.host_analysis.source.is_some());
        let manual = request_id.filter(|id| *id > 0).and_then(|request_id| {
            self.host_analysis
                .output_clock
                .as_ref()
                .filter(|clock| {
                    clock.matches_lease(lease)
                        && clock.full
                        && clock.metadata_present
                        && clock.moving_playback
                        && clock.observed.elapsed() <= LEASE_FRESHNESS
                        && clock.generation > 0
                        && clock.sequence > 0
                        && clock.queue_epoch.is_some_and(|epoch| epoch > 0)
                })
                .map(|origin| ManualSpatialNext {
                    origin: origin.clone(),
                    last_sequence: origin.sequence,
                    last_uptime_ms: origin.control_uptime_ms.unwrap(),
                    last_epoch: origin.queue_epoch.unwrap(),
                    last_media: origin.media.clone(),
                    requests: vec![ManualSpatialRequest {
                        request_id,
                        previous: false,
                        dispatch: None,
                    }],
                })
        });
        let armed = self.arm_host_spatial_next(paused_origin, None);
        if armed {
            self.host_analysis.warming_next.as_mut().unwrap().manual = manual;
        }
        armed
    }

    fn manual_spatial_scope_is_current(&self, warming: &WarmingSpatialNext) -> bool {
        self.manual_spatial_observation_is_current(warming, ManualSpatialObservation::Playing)
    }

    fn manual_spatial_observation_is_current(
        &self,
        warming: &WarmingSpatialNext,
        observation: ManualSpatialObservation,
    ) -> bool {
        let Some(manual) = &warming.manual else {
            return false;
        };
        let Some(clock) = &self.host_analysis.output_clock else {
            return false;
        };
        let Some(state) = self.remote.states.get(&warming.lease.provider) else {
            return false;
        };
        let retained_loading = observation == ManualSpatialObservation::RetainedLoading;
        let clock_proven = if retained_loading {
            clock.loading
                && clock.metadata_present
                && clock.sequence == manual.last_sequence
                && clock.control_uptime_ms == Some(manual.last_uptime_ms)
                && clock.queue_epoch == Some(manual.last_epoch)
                && clock.media == manual.last_media
                && clock.media.is_some()
                && clock.media != manual.origin.media
                && (clock.context == manual.origin.context
                    || !clock.full && clock.context.is_none() && manual.origin.context.is_some())
                && manual
                    .requests
                    .last()
                    .and_then(|request| request.dispatch.as_ref())
                    .is_some_and(|dispatch| {
                        clock.sequence > dispatch.sequence && manual.last_epoch > dispatch.epoch
                    })
        } else {
            clock.full
                && clock.metadata_present
                && clock.context == manual.origin.context
                && (clock.moving_playback
                    || observation == ManualSpatialObservation::Dispatch && clock.loading)
        };
        Instant::now() < warming.deadline
            && self.host_analysis.lease.is_none()
            && warming.natural.is_none()
            && warming.paused_origin.is_none()
            && self.remote.active.as_deref() == Some(warming.lease.provider.as_str())
            && self.remote.restored.is_none()
            && self.remote.candidate.is_none()
            && self.remote.resume_pending.is_none()
            && !self.has_pending_local_switch()
            && state.owned_output_observed
            && state.owned_output_control
            && !state.authentication_required
            && state.transport_available()
            && !state
                .devices
                .iter()
                .any(|device| device.is_active && device.id != warming.lease.device_id)
            && self
                .remote
                .providers
                .get(&warming.lease.provider)
                .is_some_and(|provider| !provider.has_pending_output_start())
            && clock.provider == manual.origin.provider
            && clock.device == manual.origin.device
            && clock.generation == manual.origin.generation
            && clock.sequence >= manual.origin.sequence
            && clock
                .control_uptime_ms
                .zip(manual.origin.control_uptime_ms)
                .is_some_and(|(now, origin)| now >= origin)
            && clock_proven
            && clock.observed.elapsed() <= LEASE_FRESHNESS
            && clock.queue_epoch.is_some_and(|epoch| epoch > 0)
            && state.playback.as_ref().is_some_and(|playback| {
                ((!retained_loading
                    && playback.state == PlaybackState::Playing
                    && playback.position_rate > 0.0)
                    || observation != ManualSpatialObservation::Playing
                        && playback.state == PlaybackState::Loading
                        && playback.position_rate == 0.0)
                    && (playback.context_uri == clock.context
                        || retained_loading
                            && clock.context.is_none()
                            && playback.context_uri == manual.origin.context)
                    && playback.current.as_ref().is_some_and(|item| {
                        self.remote
                            .providers
                            .get(&warming.lease.provider)
                            .is_some_and(|provider| {
                                clock.media.as_ref().is_some_and(|media| {
                                    provider.output_media_matches(media, &item.id)
                                })
                            })
                    })
                    && playback.device.as_ref().is_some_and(|device| {
                        device.is_active
                            && !device.is_restricted
                            && device.id == warming.lease.device_id
                    })
            })
    }

    pub(super) fn preserve_host_spatial_relative(
        &mut self,
        command: &PlayerCommand,
        request_id: Option<u64>,
    ) -> bool {
        let previous = match command {
            PlayerCommand::Next => false,
            PlayerCommand::SkipPrevious => true,
            _ => return false,
        };
        let Some(request_id) = request_id.filter(|id| *id > 0) else {
            return false;
        };
        let Some(warming) = &self.host_analysis.warming_next else {
            return false;
        };
        if !(self.manual_spatial_scope_is_current(warming)
            || self.manual_spatial_observation_is_current(
                warming,
                ManualSpatialObservation::RetainedLoading,
            ))
            || !warming.manual.as_ref().is_some_and(|manual| {
                manual.requests.len() < MANUAL_SPATIAL_REQUEST_LIMIT
                    && manual
                        .requests
                        .first()
                        .is_some_and(|request| request.dispatch.is_some())
                    && manual
                        .requests
                        .iter()
                        .all(|request| request.request_id != request_id)
            })
        {
            return false;
        }
        self.host_analysis
            .warming_next
            .as_mut()
            .unwrap()
            .manual
            .as_mut()
            .unwrap()
            .requests
            .push(ManualSpatialRequest {
                request_id,
                previous,
                dispatch: None,
            });
        // Retain only the revoked renderer. Neither this intent nor its receipt
        // renews the original deadline or restores capture/PCM authority.
        self.trace_spatial_next("manual_relative_retained", true);
        true
    }

    pub(super) fn observe_host_spatial_dispatch(&mut self, event: &PlayerEvent) {
        let PlayerEvent::RemoteOutputCommand { provider, command } = event else {
            return;
        };
        let Some(warming) = &self.host_analysis.warming_next else {
            return;
        };
        let Some(manual) = &warming.manual else {
            return;
        };
        if provider != &warming.lease.provider {
            return;
        }
        let Some(request) = manual
            .requests
            .iter()
            .find(|request| Some(request.request_id) == command.request_id)
        else {
            return;
        };
        // This observes an already issued native request. It cannot admit new
        // input or acquire a lease from a Loading frame. The provider pins the
        // dispatch occurrence independently of any enforced wire requirement.
        let scope = self
            .manual_spatial_observation_is_current(warming, ManualSpatialObservation::Dispatch)
            && request.matches_control(&command.control)
            && self
                .host_analysis
                .output_clock
                .as_ref()
                .is_some_and(|clock| {
                    command.id > 0
                        && command.generation == clock.generation
                        && command.device_id == clock.device
                        && Some(&command.media_id) == clock.media.as_ref()
                        && command.sequence >= manual.origin.sequence
                        && command.sequence <= clock.sequence
                        && command
                            .observed_queue_epoch
                            .is_some_and(|epoch| epoch > 0 && Some(epoch) == clock.queue_epoch)
                        && command
                            .required_queue_epoch
                            .is_none_or(|epoch| Some(epoch) == clock.queue_epoch)
                        && clock
                            .control_uptime_ms
                            .is_some_and(|uptime| command.expires_at_uptime_ms >= uptime)
                });
        if !scope {
            if manual.requests.len() > 1 {
                self.cancel_host_spatial_next_for("manual_dispatch_scope");
            } else {
                self.host_analysis.warming_next.as_mut().unwrap().manual = None;
            }
            return;
        }
        let dispatch = ManualSpatialDispatch {
            id: command.id,
            sequence: command.sequence,
            epoch: command.observed_queue_epoch.unwrap(),
            acknowledged: false,
        };
        let manual = self
            .host_analysis
            .warming_next
            .as_mut()
            .unwrap()
            .manual
            .as_mut()
            .unwrap();
        let request = manual
            .requests
            .iter_mut()
            .find(|request| Some(request.request_id) == command.request_id)
            .unwrap();
        if request
            .dispatch
            .as_ref()
            .is_some_and(|prior| prior.id != command.id)
        {
            self.cancel_host_spatial_next_for("manual_dispatch_duplicate");
        } else if request.dispatch.is_none() {
            request.dispatch = Some(dispatch);
        }
    }

    pub(super) fn fail_host_spatial_relative(&mut self, control: &RemoteOutputControl) {
        if matches!(
            control,
            RemoteOutputControl::Previous | RemoteOutputControl::PreviousFromStart
        ) && self
            .host_analysis
            .warming_next
            .as_ref()
            .and_then(|warming| warming.manual.as_ref())
            .is_some_and(|manual| {
                manual.requests.len() > 1 && manual.requests.iter().any(|request| request.previous)
            })
        {
            self.cancel_host_spatial_next_for("manual_previous_failed");
        }
    }

    fn continued_manual_spatial_confirms(
        &self,
        warming: &WarmingSpatialNext,
        lease: &HostAnalysisLease,
    ) -> bool {
        let Some(manual) = warming
            .manual
            .as_ref()
            .filter(|manual| manual.requests.len() > 1)
        else {
            return true;
        };
        let Some(dispatch) = manual
            .requests
            .last()
            .and_then(|request| request.dispatch.as_ref())
        else {
            return false;
        };
        self.manual_spatial_scope_is_current(warming)
            && dispatch.acknowledged
            && self
                .host_analysis
                .output_clock
                .as_ref()
                .is_some_and(|clock| {
                    clock.matches_lease(lease)
                        && clock.sequence > dispatch.sequence
                        && clock
                            .queue_epoch
                            .is_some_and(|epoch| epoch > dispatch.epoch)
                })
    }

    fn arm_host_spatial_next(
        &mut self,
        paused_origin: Option<SpatialOutputClock>,
        natural: Option<NaturalSpatialNext>,
    ) -> bool {
        let Some(source) = self.host_analysis.source.as_mut() else {
            return false;
        };
        let Some((lease, _)) = self.host_analysis.lease.take() else {
            return false;
        };
        self.host_analysis.pause_pending_until = None;
        self.host_analysis.revoked = self.host_analysis.revoked.max(lease.generation);
        if natural.is_some() {
            source.revoke_for_natural_spatial_next(lease.generation);
        } else {
            source.revoke_for_spatial_next(lease.generation);
        }
        source.set_confirmed_spatial_pause(None);
        source.set_confirmed_renderer_only_pause(None);
        self.host_analysis.renderer_only_pause = None;
        self.host_analysis.warming_next = Some(WarmingSpatialNext {
            lease,
            deadline: Instant::now() + SPATIAL_NEXT_LIMIT,
            paused_origin,
            natural,
            left_paused_origin: false,
            manual: None,
        });
        true
    }
    pub(super) fn prepare_host_spatial_seek(&mut self, request_id: u64) -> bool {
        let Some((lease, at)) = &self.host_analysis.lease else {
            return false;
        };
        let Some(origin) = self.host_analysis.output_clock.as_ref().filter(|clock| {
            clock.moving_playback
                && clock.generation > 0
                && clock.sequence > 0
                && clock.queue_epoch.is_some_and(|epoch| epoch > 0)
                && clock.observed.elapsed() <= LEASE_FRESHNESS
                && clock.matches_lease(lease)
        }) else {
            return false;
        };
        if request_id == 0
            || !lease.spatial_stereo
            || at.elapsed() > LEASE_FRESHNESS
            || !self.host_analysis_is_playing(lease)
            || self.host_spatial_status() != Some(HostSpatialStatus::Active)
            || self.has_pending_local_switch()
            || !self
                .remote
                .states
                .get(&lease.provider)
                .is_some_and(|state| state.owned_output_control && state.transport_available())
            || !self
                .owned_playback(lease)
                .is_some_and(|p| p.capabilities.seek)
            || !self
                .remote
                .providers
                .get(&lease.provider)
                .is_some_and(|provider| !provider.has_pending_output_start())
        {
            return false;
        }
        let Some(source) = &mut self.host_analysis.source else {
            return false;
        };
        let Some((lease, _)) = self.host_analysis.lease.take() else {
            return false;
        };
        self.host_analysis.pause_pending_until = None;
        self.host_analysis.revoked = self.host_analysis.revoked.max(lease.generation);
        source.revoke_for_spatial_seek(lease.generation);
        source.set_confirmed_spatial_pause(None);
        source.set_confirmed_renderer_only_pause(None);
        self.host_analysis.renderer_only_pause = None;
        self.host_analysis.warming_seek = Some(WarmingSpatialSeek {
            lease,
            origin: origin.clone(),
            request_id,
            deadline: Instant::now() + SPATIAL_NEXT_LIMIT,
        });
        true
    }
    pub(super) fn cancel_host_spatial_next(&mut self) {
        self.cancel_host_spatial_next_for("transport_or_clear");
    }

    fn cancel_host_spatial_next_for(&mut self, reason: &str) {
        if self.host_analysis.warming_next.is_some() || self.host_analysis.warming_seek.is_some() {
            self.trace_spatial_next(reason, false);
        }
        let next = self.host_analysis.warming_next.take().is_some();
        let seek = self.host_analysis.warming_seek.take().is_some();
        if (next || seek)
            && let Some(source) = &mut self.host_analysis.source
        {
            source.cancel_spatial_next();
        }
    }

    fn trace_spatial_next(&self, phase: &str, admitted: bool) {
        if std::env::var_os("AUVRA_AUDIO_TRACE").is_none_or(|value| value != "1") {
            return;
        }
        let lease = self.host_analysis.lease.as_ref();
        let playback = lease.and_then(|(lease, _)| self.owned_playback(lease));
        let state = lease.and_then(|(lease, _)| self.remote.states.get(&lease.provider));
        let clock = self.host_analysis.output_clock.as_ref();
        eprintln!(
            "auvra_spatial_next time_ms={} phase={} admitted={} lease_generation={} lease_age_ms={} lease_playing={} spatial={} owned_state={:?} owned_rate={:?} observed={} control={} transport={} next={} status={:?} sequence={} full={} clock_age_ms={} warming={} natural={}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            phase,
            admitted,
            lease.map_or(0, |(lease, _)| lease.generation),
            lease.map_or(0, |(_, at)| at.elapsed().as_millis()),
            lease.is_some_and(|(lease, _)| lease.playing),
            lease.is_some_and(|(lease, _)| lease.spatial_stereo),
            playback.map(|p| p.state),
            playback.map(|p| p.position_rate),
            state.is_some_and(|s| s.owned_output_observed),
            state.is_some_and(|s| s.owned_output_control),
            state.is_some_and(|s| s.transport_available()),
            playback.is_some_and(|p| p.transport.next),
            self.host_spatial_status(),
            clock.map_or(0, |c| c.sequence),
            clock.is_some_and(|c| c.full),
            clock.map_or(0, |c| c.observed.elapsed().as_millis()),
            self.host_analysis.warming_next.is_some(),
            self.host_analysis
                .warming_next
                .as_ref()
                .is_some_and(|w| w.natural.is_some()),
        );
    }

    fn trace_successor_first_rejection(
        &mut self,
        lease_generation: u64,
        origin: &SpatialOutputClock,
        incoming: &RemoteOutputClock,
        duration_ms: u64,
        projection: Option<TailProjection>,
        context_matches: bool,
    ) {
        if std::env::var_os("AUVRA_AUDIO_TRACE").is_none_or(|value| value != "1") {
            return;
        }
        let Some(successor) = incoming.media_id.as_ref() else {
            return;
        };
        let scope = SuccessorRejectScope {
            provider: origin.provider.clone(),
            lease_generation,
            runtime_generation: incoming.generation,
            successor: successor.clone(),
        };
        if self.host_analysis.last_successor_reject.as_ref() == Some(&scope) {
            return;
        }
        self.host_analysis.last_successor_reject = Some(scope);
        let queue = self.host_analysis.successor_queue.as_ref();
        let control_delta_ms = projection.map(|value| value.control_delta_ms).or_else(|| {
            incoming
                .control_uptime_ms
                .zip(origin.control_uptime_ms)
                .and_then(|(next, old)| next.checked_sub(old))
        });
        let tail_start =
            duration_ms.saturating_sub(NATURAL_SPATIAL_TAIL_MS + SUCCESSOR_FIRST_POSITION_SKEW_MS);
        let tail_end = duration_ms.saturating_add(250);
        eprintln!(
            "auvra_natural_reject time_ms={} lease_generation={} runtime_generation={} origin_sequence={} incoming_sequence={} origin_position_ms={} projected_position_ms={} duration_ms={} origin_age_ms={} control_delta_ms={} applied_delta_ms={} rate={} raw_in_tail={} projected_in_tail={} incoming_position_ms={} incoming_near_start={} origin_epoch={} incoming_epoch={} epoch_advances={} context_matches={} queue_present={} queue_age_ms={} queue_fresh={} queue_head_matches={} queue_epoch_matches={} queue_scope_matches={} outgoing_tail_window_ms={}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            lease_generation,
            incoming.generation,
            origin.sequence,
            incoming.sequence,
            origin.position_ms,
            projection.map_or(-1, |value| i128::from(value.position_ms)),
            duration_ms,
            origin.observed.elapsed().as_millis(),
            control_delta_ms.map_or(-1, i128::from),
            projection.map_or(-1, |value| i128::from(value.applied_delta_ms)),
            origin.rate,
            (tail_start..=tail_end).contains(&origin.position_ms),
            projection.is_some_and(|value| (tail_start..=tail_end).contains(&value.position_ms)),
            incoming.position_ms,
            incoming.position_ms <= NATURAL_SPATIAL_TAIL_MS,
            origin.queue_epoch.unwrap_or_default(),
            incoming.queue_epoch.unwrap_or_default(),
            incoming
                .queue_epoch
                .zip(origin.queue_epoch)
                .is_some_and(|(next, old)| next > old),
            context_matches,
            queue.is_some(),
            queue.map_or(-1, |queue| queue.observed.elapsed().as_millis() as i128),
            queue.is_some_and(|queue| queue.observed.elapsed() <= SPATIAL_QUEUE_FRESHNESS),
            queue.is_some_and(|queue| &queue.successor == successor),
            queue.is_some_and(|queue| Some(queue.epoch) == origin.queue_epoch),
            queue.is_some_and(|queue| queue.provider == origin.provider
                && queue.generation == origin.generation
                && queue.device == origin.device
                && Some(&queue.media) == origin.media.as_ref()),
            NATURAL_SPATIAL_TAIL_MS + SUCCESSOR_FIRST_POSITION_SKEW_MS,
        );
    }

    fn observe_spatial_successor_queue(&mut self, provider: &str, queue: &RemoteOutputQueue) {
        let valid = self
            .host_analysis
            .lease
            .as_ref()
            .is_some_and(|(lease, at)| {
                provider == lease.provider
                    && lease.spatial_stereo
                    && at.elapsed() <= LEASE_FRESHNESS
                    && self.host_analysis_is_playing(lease)
                    && self.remote.states.get(provider).is_some_and(|state| {
                        state.owned_output_control && state.transport_available()
                    })
                    && self
                        .host_analysis
                        .output_clock
                        .as_ref()
                        .is_some_and(|clock| {
                            clock.matches_lease(lease)
                                && clock.observed.elapsed() <= LEASE_FRESHNESS
                                && clock.generation == queue.generation
                                && clock.queue_epoch == Some(queue.epoch)
                                && clock.device == queue.device_id
                                && clock.media.as_ref() == Some(&queue.media_id)
                        })
            });
        if !valid
            || queue.sequence == 0
            || queue.epoch == 0
            || queue.generation == 0
            || queue.upcoming.len() > 10
            || queue.previous.len() > 10
            || self
                .host_analysis
                .successor_queue_watermarks
                .get(provider)
                .is_some_and(|old| (queue.generation, queue.sequence) <= *old)
        {
            return;
        }
        // Empty queues revoke the head too. Keep their watermark across transport clears,
        // so a delayed older queue cannot restore a withdrawn successor.
        self.host_analysis
            .successor_queue_watermarks
            .insert(provider.into(), (queue.generation, queue.sequence));
        // Keep only one bounded, exact native head, not a second queue or an HTTP prediction.
        self.host_analysis.successor_queue =
            queue.upcoming.first().map(|entry| SpatialSuccessorQueue {
                provider: provider.into(),
                generation: queue.generation,
                device: queue.device_id.clone(),
                media: queue.media_id.clone(),
                epoch: queue.epoch,
                successor: entry.media_id.clone(),
                observed: Instant::now(),
            });
    }

    fn prepare_natural_spatial_next(&mut self, provider: &str, clock: &RemoteOutputClock) {
        if !clock.active
            || clock.control_uptime_ms.is_none()
            || clock.state != PlaybackState::Loading
            || clock.rate != 0.0
            || self.host_analysis.warming_next.is_some()
            || self.host_analysis.warming_seek.is_some()
            || self.has_pending_local_switch()
            || self.host_spatial_status() != Some(HostSpatialStatus::Active)
        {
            return;
        }
        let Some((lease, _)) = &self.host_analysis.lease else {
            return;
        };
        let Some(origin) = self.host_analysis.output_clock.as_ref().filter(|origin| {
            origin.matches_lease(lease)
                && origin.full
                && origin.moving_playback
                && origin.observed.elapsed() <= LEASE_FRESHNESS
                && origin.generation == clock.generation
                && origin.sequence < clock.sequence
                && origin.queue_epoch.is_some_and(|epoch| epoch > 0)
        }) else {
            return;
        };
        if provider != lease.provider
            || clock.device_id != lease.device_id
            || !self.host_analysis_is_playing(lease)
            || !self.remote.states.get(provider).is_some_and(|state| {
                state.connection == ProviderConnectionState::Connected && state.owned_output_control
            })
        {
            return;
        }
        let Some(playback) = self.owned_playback(lease) else {
            return;
        };
        let Some((track, context)) = playback.current.as_ref().zip(playback.context_uri.as_ref())
        else {
            return;
        };
        if track.duration_ms < NATURAL_SPATIAL_TAIL_MS
            || !playback.transport.next
            || playback.repeat == RepeatMode::One
            || origin.context.as_ref() != Some(context)
        {
            return;
        }
        let incoming_context = clock
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.context_uri.as_ref());
        let successor = if clock.media_id.as_ref() == Some(&lease.media_id) {
            if origin.queue_epoch != clock.queue_epoch
                || clock.position_ms < track.duration_ms - NATURAL_SPATIAL_TAIL_MS
                || clock.position_ms > track.duration_ms.saturating_add(250)
                || incoming_context != Some(context)
            {
                return;
            }
            None
        } else {
            // Soloist may emit track_changed(new URI) before any Loading(old URI).
            // A fresh outgoing tail plus the exact native queue head authorizes only zero preroll.
            // A different observed successor permits bounded outgoing metadata
            // skew; it never enlarges the incoming start or same-URI windows.
            // Full Playing and the original context are still required to accept the new lease.
            let Some(media) = clock.media_id.as_ref() else {
                return;
            };
            let projection = origin.projected_tail_position(clock, Instant::now());
            let context_matches = incoming_context.is_none_or(|incoming| incoming == context);
            if !projection.is_some_and(|value| {
                value.position_ms
                    >= track
                        .duration_ms
                        .saturating_sub(NATURAL_SPATIAL_TAIL_MS + SUCCESSOR_FIRST_POSITION_SKEW_MS)
                    && value.position_ms <= track.duration_ms.saturating_add(250)
            }) || clock.position_ms > NATURAL_SPATIAL_TAIL_MS
                || clock
                    .queue_epoch
                    .zip(origin.queue_epoch)
                    .is_none_or(|(next, old)| next <= old)
                || !context_matches
                || !self
                    .host_analysis
                    .successor_queue
                    .as_ref()
                    .is_some_and(|queue| queue.confirms(origin, media))
            {
                let lease_generation = lease.generation;
                let duration_ms = track.duration_ms;
                let origin = origin.clone();
                self.trace_successor_first_rejection(
                    lease_generation,
                    &origin,
                    clock,
                    duration_ms,
                    projection,
                    context_matches,
                );
                self.trace_spatial_next("successor_first_rejected", false);
                return;
            }
            if let Some(projection) = projection
                && track.duration_ms.saturating_sub(projection.position_ms)
                    > NATURAL_SPATIAL_TAIL_MS
                && std::env::var_os("AUVRA_AUDIO_TRACE").is_some_and(|value| value == "1")
            {
                eprintln!(
                    "auvra_natural_tolerance time_ms={} duration_ms={} projected_position_ms={} remaining_ms={}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                    track.duration_ms,
                    projection.position_ms,
                    track.duration_ms - projection.position_ms,
                );
            }
            Some((media.clone(), None))
        };
        self.trace_spatial_next(
            if successor.is_some() {
                "successor_first_admitted"
            } else {
                "outgoing_tail_admitted"
            },
            true,
        );
        self.arm_host_spatial_next(
            None,
            Some(NaturalSpatialNext {
                origin: origin.clone(),
                context: context.clone(),
                successor,
            }),
        );
    }

    pub(super) fn check_host_spatial_clock(&mut self, command: &PlayerCommand) {
        if let PlayerCommand::Remote {
            provider,
            action: RemoteAction::OutputCommandResult { result },
        } = command
        {
            let Some(warming) = &mut self.host_analysis.warming_next else {
                return;
            };
            if provider != &warming.lease.provider {
                return;
            }
            let Some(manual) = &mut warming.manual else {
                return;
            };
            if result.generation != manual.origin.generation {
                return;
            }
            let Some(dispatch) = manual
                .requests
                .iter_mut()
                .filter_map(|request| request.dispatch.as_mut())
                .find(|dispatch| dispatch.id == result.id)
            else {
                return;
            };
            if result.accepted {
                dispatch.acknowledged = true;
            } else {
                self.cancel_host_spatial_next_for("manual_dispatch_rejected");
            }
            return;
        }
        if let PlayerCommand::Remote {
            provider,
            action: RemoteAction::OutputQueue { queue },
        } = command
        {
            self.observe_spatial_successor_queue(provider, queue);
            return;
        }
        let PlayerCommand::Remote {
            provider,
            action: RemoteAction::OutputClock { clock },
        } = command
        else {
            return;
        };
        if let Some(clock) = clock {
            self.prepare_natural_spatial_next(provider, clock);
        }
        let warming_seek = self
            .host_analysis
            .warming_seek
            .as_ref()
            .filter(|warming| &warming.lease.provider == provider);
        if warming_seek.is_some_and(|warming| {
            !clock
                .as_ref()
                .is_some_and(|clock| warming.matches_clock(clock))
        }) {
            self.cancel_host_spatial_next_for("seek_clock_scope");
            self.host_analysis.output_clock = None;
            return;
        }
        // The first full Playing clock can precede acquisition of its host lease.
        if self.remote.active.as_deref() == Some(provider.as_str()) {
            match clock {
                Some(clock)
                    if self.host_analysis.output_clock.as_ref().is_none_or(|last| {
                        last.provider != *provider
                            || (clock.generation, clock.sequence) > (last.generation, last.sequence)
                    }) =>
                {
                    self.host_analysis.output_clock =
                        Some(SpatialOutputClock::new(provider, clock));
                }
                None => self.host_analysis.output_clock = None,
                _ => {}
            }
        }
        let Some(warming) = &mut self.host_analysis.warming_next else {
            return;
        };
        if &warming.lease.provider != provider {
            return;
        }
        if let Some(natural) = &mut warming.natural {
            let retained = clock.as_ref().is_some_and(|clock| natural.observe(clock));
            if std::env::var_os("AUVRA_AUDIO_TRACE").is_some_and(|value| value == "1") {
                if let Some(clock) = clock {
                    eprintln!(
                        "auvra_natural sequence={} epoch={} state={:?} full={} retained={}",
                        clock.sequence,
                        clock.queue_epoch.unwrap_or_default(),
                        clock.state,
                        clock.actions.is_some(),
                        retained,
                    );
                } else {
                    eprintln!("auvra_natural clock_cleared");
                }
            }
            if !retained {
                self.cancel_host_spatial_next_for("natural_clock_scope");
            }
            return;
        }
        let keeps_warming = clock.as_ref().is_some_and(|clock| {
            if !clock.active || clock.device_id != warming.lease.device_id {
                return false;
            }
            if let Some(manual) = &mut warming.manual
                && !manual.retains_clock(clock)
            {
                return false;
            }
            if let Some(paused) = &mut warming.paused_origin {
                if clock.generation != paused.generation {
                    return false;
                }
                if clock.state == PlaybackState::Paused {
                    let same_pause = !warming.left_paused_origin
                        && clock.media_id == paused.media
                        && clock.queue_epoch == paused.queue_epoch
                        && SpatialOutputClock::stationary_pause(clock);
                    if same_pause && clock.sequence > paused.sequence {
                        paused.sequence = clock.sequence;
                        paused.observed = Instant::now();
                    }
                    return same_pause && paused.observed.elapsed() <= LEASE_FRESHNESS;
                }
                warming.left_paused_origin |= clock.state == PlaybackState::Playing
                    || clock.queue_epoch != paused.queue_epoch
                    || clock
                        .media_id
                        .as_ref()
                        .is_some_and(|id| Some(id) != paused.media.as_ref());
            }
            matches!(clock.state, PlaybackState::Playing | PlaybackState::Loading)
        });
        if !keeps_warming {
            self.cancel_host_spatial_next_for("manual_clock_scope");
        }
    }
    pub(super) fn update_host_analysis(
        &mut self,
        lease: Option<HostAnalysisLease>,
    ) -> Result<(), PlayerError> {
        self.accept_host_analysis(lease, None)
    }
    pub(super) fn retire_host_analysis_generation(
        &mut self,
        generation: u64,
    ) -> Result<(), PlayerError> {
        if generation == 0 {
            return Err(PlayerError::invalid("Invalid retired analysis generation"));
        }
        if let Some((lease, _)) = &self.host_analysis.lease {
            if lease.generation == generation {
                self.clear_host_analysis();
            }
            // A late release of an old generation never revokes a newer lease.
            return Ok(());
        }
        let Some(warming) = self
            .host_analysis
            .warming_next
            .as_ref()
            .filter(|w| w.lease.generation == generation)
        else {
            return Ok(());
        };
        let preserve = Instant::now() < warming.deadline
            && self.remote.active.as_deref() == Some(warming.lease.provider.as_str())
            && warming.natural.as_ref().is_some_and(|natural| {
                self.host_analysis
                    .output_clock
                    .as_ref()
                    .is_some_and(|clock| natural.matches_host_retirement(clock))
            })
            && self
                .remote
                .states
                .get(&warming.lease.provider)
                .is_some_and(|state| {
                    state.owned_output_observed
                        && !state.authentication_required
                        && state.transport_available()
                        && state.playback.as_ref().is_some_and(|playback| {
                            matches!(
                                playback.state,
                                PlaybackState::Playing | PlaybackState::Loading
                            ) && playback.device.as_ref().is_some_and(|device| {
                                device.id == warming.lease.device_id
                                    && device.is_active
                                    && !device.is_restricted
                            })
                        })
                });
        self.trace_spatial_next(
            if preserve {
                "old_host_generation_retired"
            } else {
                "host_retirement_rejected"
            },
            preserve,
        );
        if !preserve {
            self.clear_host_analysis();
        }
        // This command never installs a lease, opens capture, or promotes a successor.
        // Fresh full Playing plus the separately verified new host lease still do that.
        Ok(())
    }
    pub(super) fn update_host_analysis_after_seek(
        &mut self,
        lease: HostAnalysisLease,
        request_id: u64,
        sequence: u64,
    ) -> Result<(), PlayerError> {
        self.accept_host_analysis(Some(lease), Some((request_id, sequence)))
    }
    pub(super) fn note_applied_host_output_clock(
        &mut self,
        command: &PlayerCommand,
        admitted: bool,
    ) {
        let PlayerCommand::Remote {
            provider,
            action: RemoteAction::OutputClock { clock },
        } = command
        else {
            return;
        };
        if self.remote.active.as_ref() != Some(provider) {
            return;
        }
        self.host_analysis.applied_output_clock = clock
            .as_ref()
            .filter(|clock| {
                admitted
                    && self
                        .host_analysis
                        .output_clock
                        .as_ref()
                        .is_some_and(|known| {
                            known.provider == *provider
                                && (known.generation, known.sequence)
                                    == (clock.generation, clock.sequence)
                        })
            })
            .map(|clock| (clock.generation, clock.sequence));
    }
    pub(super) fn update_host_analysis_after_clock_change(
        &mut self,
        lease: HostAnalysisLease,
        retired_generation: u64,
        origin_sequence: u64,
        sequence: u64,
    ) -> Result<(), PlayerError> {
        if !lease.is_valid() {
            return Err(PlayerError::invalid("Invalid owned analysis lease"));
        }
        let Some((old, _)) = self.host_analysis.lease.as_ref().filter(|(old, _)| {
            old.generation == retired_generation
                && lease.generation > retired_generation
                && lease.provider == old.provider
                && lease.media_id == old.media_id
                && lease.device_id == old.device_id
                && lease.process_id == old.process_id
                && lease.spatial_stereo
                && old.spatial_stereo
        }) else {
            // The paired command has no authority after its old lease is gone.
            return Ok(());
        };
        if !lease.playing
            || !self.host_analysis_is_playing(old)
            || self.host_analysis.pause_pending_until.is_some()
            || self.host_analysis.deferred_play.is_some()
            || self.has_pending_local_switch()
            || !self
                .remote
                .providers
                .get(&lease.provider)
                .is_some_and(|provider| !provider.has_pending_output_start())
        {
            // A subsequent heartbeat for this superseded generation must not
            // restart the renderer while an explicit transport action is pending.
            self.host_analysis.revoked = self.host_analysis.revoked.max(lease.generation);
            return Ok(());
        }
        let exact_clock = self
            .host_analysis
            .output_clock
            .as_ref()
            .is_some_and(|clock| {
                clock.matches_lease(&lease)
                    && clock.sequence == sequence
                    && self.host_analysis.applied_output_clock == Some((clock.generation, sequence))
            });
        let now = Instant::now();
        let retain_renderer =
            self.can_reanchor_spatial_output(old, &lease, origin_sequence, sequence, now);
        if retain_renderer {
            self.host_analysis.revoked = self.host_analysis.revoked.max(retired_generation);
            self.host_analysis.lease = None;
            self.host_analysis.leased_output_clock = None;
            self.host_analysis.renderer_only_pause = None;
            if let Some(source) = self.host_analysis.source.as_mut() {
                // Old capture and analysis end before publishing the next lease.
                // The adapter may hold its verified clock and enqueue silence.
                source.revoke_for_spatial_reanchor(
                    retired_generation,
                    lease.generation,
                    now + SPATIAL_NEXT_LIMIT,
                );
            }
            self.trace_spatial_next("same_occurrence_clock_change", true);
        } else {
            self.clear_host_analysis();
            if !exact_clock {
                return Ok(());
            }
        }
        self.accept_host_analysis(Some(lease), None)
    }
    fn can_reanchor_spatial_output(
        &self,
        old: &HostAnalysisLease,
        lease: &HostAnalysisLease,
        origin_sequence: u64,
        sequence: u64,
        now: Instant,
    ) -> bool {
        if self.host_spatial_status() != Some(HostSpatialStatus::Active)
            || self.host_analysis.warming_next.is_some()
            || self.host_analysis.warming_seek.is_some()
        {
            return false;
        }
        let Some((origin, incoming)) = self
            .host_analysis
            .leased_output_clock
            .as_ref()
            .zip(self.host_analysis.output_clock.as_ref())
        else {
            return false;
        };
        let authority = self.capture_authority(old, origin);
        let control_delta_ms = incoming
            .control_uptime_ms
            .zip(origin.control_uptime_ms)
            .and_then(|(incoming, origin)| incoming.checked_sub(origin));
        authority.is_some()
            && self.capture_authority(lease, incoming) == authority
            && origin.sequence == origin_sequence
            && incoming.sequence == sequence
            && sequence > origin_sequence
            && self.host_analysis.applied_output_clock == Some((incoming.generation, sequence))
            && incoming.generation == origin.generation
            && incoming.matches_lease(lease)
            && incoming.full
            && origin.full
            && origin.metadata_present
            && incoming.metadata_present
            && origin.moving_playback
            && incoming.moving_playback
            && incoming.rate == origin.rate
            && origin.context.is_some()
            && origin.context == incoming.context
            && origin.queue_epoch.is_some_and(|epoch| epoch > 0)
            && origin.queue_epoch == incoming.queue_epoch
            && control_delta_ms.is_some_and(|elapsed| {
                let age = now.saturating_duration_since(origin.observed);
                let advance = elapsed.min(age.as_millis() as u64) as f64 * origin.rate;
                Duration::from_millis(elapsed) <= LEASE_FRESHNESS
                    && (origin.position_ms as f64 + advance - incoming.position_ms as f64).abs()
                        > 250.0
            })
    }
    fn accept_host_analysis(
        &mut self,
        lease: Option<HostAnalysisLease>,
        seek_proof: Option<(u64, u64)>,
    ) -> Result<(), PlayerError> {
        if let Some(lease) = lease {
            if !lease.is_valid() {
                return Err(PlayerError::invalid("Invalid owned analysis lease"));
            }
            self.revoke_stale_host_analysis();
            // A queued heartbeat cannot revive a lease invalidated by transport intent.
            if lease.generation <= self.host_analysis.revoked {
                return Ok(());
            }
            if let Some((old, _)) = &self.host_analysis.lease
                && (lease.generation < old.generation
                    || lease.generation == old.generation && !lease.same_output(old))
            {
                return Ok(());
            }
            let now = Instant::now();
            let existing_output = self
                .host_analysis
                .lease
                .as_ref()
                .is_some_and(|(old, _)| lease.same_output(old));
            if !existing_output && (!lease.playing || !self.owned_playback_is_playing(&lease)) {
                // Only actual playback may acquire a new route, including after restore.
                if self.host_analysis.lease.is_none() {
                    self.host_analysis.revoked = self.host_analysis.revoked.max(lease.generation);
                }
                return Ok(());
            }
            if !existing_output {
                self.host_analysis.pause_pending_until = None;
                if self
                    .host_analysis
                    .warming_next
                    .as_ref()
                    .is_some_and(|warming| !self.continued_manual_spatial_confirms(warming, &lease))
                {
                    // An intermediate/repeated URI cannot claim the muted
                    // standby before the latest dispatched occurrence arrives.
                    self.trace_spatial_next("manual_latest_not_confirmed", false);
                    return Ok(());
                }
                if let Some(warming) = &self.host_analysis.warming_seek {
                    let confirmed = seek_proof.is_some_and(|(request_id, sequence)| {
                        self.host_analysis
                            .output_clock
                            .as_ref()
                            .is_some_and(|clock| {
                                warming.confirms(&lease, clock, request_id, sequence)
                            })
                    });
                    if confirmed {
                        self.host_analysis.warming_seek = None;
                    } else {
                        self.cancel_host_spatial_next_for("seek_lease_not_confirmed");
                    }
                }
                if let Some(warming) = &self.host_analysis.warming_next {
                    let old = &warming.lease;
                    if lease.spatial_stereo
                        && lease.provider == old.provider
                        && lease.device_id == old.device_id
                        && lease.process_id == old.process_id
                        && lease.media_id != old.media_id
                        && warming.natural.as_ref().is_none_or(|natural| {
                            self.host_analysis
                                .output_clock
                                .as_ref()
                                .is_some_and(|clock| natural.confirms(&lease, clock))
                        })
                    {
                        // The worker may consume the renderer after independently verifying the new lease.
                        self.trace_spatial_next("successor_lease_confirmed", true);
                        self.host_analysis.warming_next = None;
                    } else {
                        self.cancel_host_spatial_next_for("successor_lease_scope");
                    }
                }
            }
            self.host_analysis.leased_output_clock = self
                .host_analysis
                .output_clock
                .as_ref()
                .filter(|clock| {
                    lease.spatial_stereo
                        && lease.playing
                        && clock.matches_lease(&lease)
                        && clock.full
                        && clock.moving_playback
                        && self.host_analysis.applied_output_clock
                            == Some((clock.generation, clock.sequence))
                })
                .cloned();
            self.host_analysis.lease = Some((lease, now));
            if let Some(playing) = self
                .host_analysis
                .lease
                .as_ref()
                .map(|(lease, _)| lease.clone())
                .filter(|lease| lease.playing)
            {
                // Pause can arrive before the next analysis poll. Record actual full Playing
                // ownership at lease acceptance, never reconstruct it from a paused restore.
                self.renderer_only_pause_proof(&playing, false);
            }
            if self
                .host_analysis
                .resume_clock_gap
                .as_ref()
                .is_some_and(|gap| {
                    self.host_analysis
                        .lease
                        .as_ref()
                        .is_none_or(|(lease, _)| lease.playing || !lease.same_output(&gap.lease))
                })
            {
                self.host_analysis.resume_clock_gap = None;
            }
        } else {
            self.clear_host_analysis();
        }
        Ok(())
    }
    pub(super) fn host_visuals_are_current(&self) -> bool {
        self.host_analysis
            .lease
            .as_ref()
            .is_some_and(|(lease, at)| {
                lease.visuals
                    && at.elapsed() <= LEASE_FRESHNESS
                    && self.host_analysis_is_playing(lease)
            })
    }
    pub(super) fn host_spatial_status(&self) -> Option<HostSpatialStatus> {
        if !self.host_analysis_is_current()
            || !self
                .host_analysis
                .lease
                .as_ref()
                .is_some_and(|(lease, _)| lease.spatial_stereo)
        {
            return None;
        }
        self.host_analysis
            .source
            .as_ref()
            .map(|source| source.spatial_status())
    }
    pub(super) fn host_analysis_is_current(&self) -> bool {
        let Some((lease, observed)) = &self.host_analysis.lease else {
            return false;
        };
        observed.elapsed() <= LEASE_FRESHNESS
            && match self.owned_playback(lease).map(|playback| playback.state) {
                Some(PlaybackState::Playing) => true,
                Some(PlaybackState::Paused | PlaybackState::Loading) => lease.spatial_stereo,
                _ => false,
            }
    }
    fn host_analysis_is_playing(&self, lease: &HostAnalysisLease) -> bool {
        lease.playing
            && self
                .host_analysis
                .pause_pending_until
                .is_none_or(|deadline| Instant::now() >= deadline)
            && self.owned_playback_is_playing(lease)
    }
    fn owned_playback_is_playing(&self, lease: &HostAnalysisLease) -> bool {
        self.owned_playback(lease).is_some_and(|playback| {
            playback.state == PlaybackState::Playing
                && (!lease.spatial_stereo || playback.position_rate > 0.0)
        })
    }
    fn owned_playback(&self, lease: &HostAnalysisLease) -> Option<&RemotePlayback> {
        if self.remote.active.as_deref() != Some(&lease.provider)
            || self.remote.restored.is_some()
            || self.remote.resume_pending.is_some()
        {
            return None;
        }
        let state = self.remote.states.get(&lease.provider)?;
        if !matches!(
            state.connection,
            ProviderConnectionState::Connected | ProviderConnectionState::Unavailable
        ) || !state.owned_output_observed
            || state.authentication_required
        {
            return None;
        }
        let playback = state.playback.as_ref()?;
        let device = playback.device.as_ref()?;
        (device.id == lease.device_id
            && device.is_active
            && !device.is_restricted
            && playback.current.as_ref()?.id == lease.media_id)
            .then_some(playback)
    }
    pub(super) fn preserve_paused_spatial_seek(&mut self, command: &PlayerCommand) -> bool {
        let Some((lease, _)) = self.host_analysis.lease.as_ref() else {
            return false;
        };
        let is_seek = match command {
            PlayerCommand::Seek { .. } => true,
            PlayerCommand::Remote {
                provider,
                action:
                    RemoteAction::Control {
                        control: RemoteControl::Seek { .. },
                    },
            } => provider == &lease.provider,
            _ => false,
        };
        // A seek cannot acquire a route. An already suspended relay has no
        // presented audio or queued samples to carry across a paused seek.
        let preserve = is_seek
            && !lease.playing
            && self.host_analysis.deferred_play.is_none()
            && self.host_analysis.warming_next.is_none()
            && self.host_analysis.warming_seek.is_none()
            && !self.has_pending_local_switch()
            && self.host_spatial_status() == Some(HostSpatialStatus::Paused)
            && self.owned_playback(lease).is_some_and(|playback| {
                playback.state == PlaybackState::Paused
                    && playback.position_rate == 0.0
                    && playback.capabilities.seek
            })
            && self
                .remote
                .states
                .get(&lease.provider)
                .is_some_and(|state| state.owned_output_control && state.transport_available())
            && self
                .remote
                .providers
                .get(&lease.provider)
                .is_some_and(|provider| !provider.has_pending_output_start());
        if preserve {
            let generation = lease.generation;
            if let Some(source) = &mut self.host_analysis.source {
                source.invalidate_spatial_resume_after_seek(generation);
            }
        }
        preserve
    }
    pub(super) fn preserve_host_spatial_transport(
        &mut self,
        command: &PlayerCommand,
    ) -> Option<PlaybackState> {
        let (lease, _) = self.host_analysis.lease.as_ref()?;
        if !lease.spatial_stereo || !self.host_analysis_is_current() {
            return None;
        }
        let transport = match command {
            PlayerCommand::Play => PlaybackState::Playing,
            PlayerCommand::Pause => PlaybackState::Paused,
            PlayerCommand::Remote {
                provider,
                action: RemoteAction::Control { control },
            } if provider == &lease.provider => match control {
                RemoteControl::Play => PlaybackState::Playing,
                RemoteControl::Pause => PlaybackState::Paused,
                RemoteControl::PauseOnDevice { device_id } if device_id == &lease.device_id => {
                    PlaybackState::Paused
                }
                _ => return None,
            },
            _ => return None,
        };
        if transport == PlaybackState::Paused {
            self.host_analysis.resume_clock_gap = None;
            // Mute before dispatching Pause. A rejection or missing receipt must
            // not leave an otherwise still-playing output muted indefinitely.
            self.host_analysis.pause_pending_until = Some(Instant::now() + LEASE_FRESHNESS);
            self.poll_host_analysis();
        } else {
            self.arm_spatial_resume_clock_gap();
        }
        Some(transport)
    }
    fn arm_spatial_resume_clock_gap(&mut self) {
        let Some((lease, _)) = self.host_analysis.lease.as_ref().filter(|(lease, _)| {
            !lease.playing
                && self.owned_playback(lease).is_some_and(|playback| {
                    playback.state == PlaybackState::Paused && playback.position_rate == 0.0
                })
        }) else {
            self.trace_spatial_resume_clock_gap("arm_rejected_not_paused");
            return;
        };
        let Some(paused) = self.host_analysis.output_clock.as_ref().filter(|clock| {
            clock.stationary_pause
                && clock.matches_lease(lease)
                && clock.observed.elapsed() <= LEASE_FRESHNESS
                && clock.queue_epoch.is_some_and(|epoch| {
                    self.host_analysis
                        .renderer_only_pause
                        .as_ref()
                        .is_some_and(|origin| {
                            origin.confirmed
                                && Some(origin.authority) == self.capture_authority(lease, clock)
                                && origin.matches(lease, clock, epoch)
                        })
                })
        }) else {
            self.trace_spatial_resume_clock_gap("arm_rejected_clock_or_proof");
            return;
        };
        // The confirmed origin is the authority. A later stationary partial
        // heartbeat preserves it but cannot create a new paused proof.
        // Explicit Play can bridge its first incomplete status event only.
        // Heartbeats and repeated polling never renew this original deadline.
        if self
            .host_analysis
            .resume_clock_gap
            .as_ref()
            .is_none_or(|gap| {
                Instant::now() >= gap.deadline
                    || !gap.lease.same_output(lease)
                    || gap.paused.generation != paused.generation
                    || gap.paused.queue_epoch != paused.queue_epoch
            })
        {
            self.host_analysis.resume_clock_gap = Some(SpatialResumeClockGap {
                lease: lease.clone(),
                paused: paused.clone(),
                deadline: Instant::now() + LEASE_FRESHNESS,
            });
        }
        self.trace_spatial_resume_clock_gap("armed");
    }
    fn trace_spatial_resume_clock_gap(&self, phase: &str) {
        if std::env::var_os("AUVRA_AUDIO_TRACE").is_none_or(|value| value != "1") {
            return;
        }
        let lease = self.host_analysis.lease.as_ref().map(|(lease, _)| lease);
        let clock = self.host_analysis.output_clock.as_ref();
        let origin = self.host_analysis.renderer_only_pause.as_ref();
        let gap = self.host_analysis.resume_clock_gap.as_ref();
        eprintln!(
            "auvra_resume_gap time_ms={} phase={} armed={} lease_generation={} lease_playing={} sequence={} full={} stationary={} moving={} partial_zero={} clock_age_ms={} prior_confirmed={} same_output={} same_origin={} remaining_ms={} pause_pending={} owned_state={:?} owned_rate={:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            phase,
            gap.is_some(),
            lease.map_or(0, |lease| lease.generation),
            lease.is_some_and(|lease| lease.playing),
            clock.map_or(0, |clock| clock.sequence),
            clock.is_some_and(|clock| clock.full),
            clock.is_some_and(|clock| clock.stationary_pause),
            clock.is_some_and(|clock| clock.moving_playback),
            clock.is_some_and(|clock| clock.incomplete_zero_playback),
            clock.map_or(0, |clock| clock.observed.elapsed().as_millis()),
            origin.is_some_and(|origin| origin.confirmed),
            lease
                .zip(clock)
                .is_some_and(|(lease, clock)| clock.matches_lease(lease)),
            lease
                .zip(clock)
                .zip(origin)
                .is_some_and(|((lease, clock), origin)| {
                    clock
                        .queue_epoch
                        .is_some_and(|epoch| origin.matches(lease, clock, epoch))
                }),
            gap.map_or(0, |gap| gap
                .deadline
                .saturating_duration_since(Instant::now())
                .as_millis()),
            self.host_analysis.pause_pending_until.is_some(),
            lease
                .and_then(|lease| self.owned_playback(lease))
                .map(|playback| playback.state),
            lease
                .and_then(|lease| self.owned_playback(lease))
                .map(|playback| playback.position_rate),
        );
    }
    pub(super) fn cancel_spatial_resume_clock_gap(&mut self) {
        self.host_analysis.resume_clock_gap = None;
    }
    pub(super) fn cancel_host_spatial_pause(&mut self) {
        self.host_analysis.pause_pending_until = None;
    }
    pub(super) fn defer_host_spatial_resume(&mut self, request_id: Option<u64>) -> bool {
        if self.host_analysis.deferred_play.is_some() {
            return true;
        }
        let Some((lease, _)) = self.host_analysis.lease.as_ref() else {
            return false;
        };
        if !self
            .owned_playback(lease)
            .is_some_and(|playback| playback.state == PlaybackState::Paused)
            || !self
                .host_analysis
                .source
                .as_mut()
                .is_some_and(|source| source.prepare_spatial_resume(lease.generation))
        {
            return false;
        }
        self.host_analysis.deferred_play = Some(DeferredSpatialPlay {
            lease: lease.clone(),
            deadline: Instant::now() + SPATIAL_RESUME_LIMIT,
            request_id,
        });
        true
    }
    pub(super) fn cancel_deferred_spatial_play(&mut self) {
        self.cancel_spatial_resume_clock_gap();
        self.host_analysis.deferred_play = None;
        if let Some(source) = &mut self.host_analysis.source {
            source.cancel_spatial_resume();
        }
    }
    pub(crate) fn poll_deferred_spatial_play(&mut self) {
        let Some(pending) = self.host_analysis.deferred_play.as_ref() else {
            return;
        };
        let Some(playback) = self.owned_playback(&pending.lease) else {
            self.cancel_deferred_spatial_play();
            return;
        };
        let already_playing =
            playback.state == PlaybackState::Playing && playback.position_rate > 0.0;
        if !matches!(
            playback.state,
            PlaybackState::Paused | PlaybackState::Loading | PlaybackState::Playing
        ) {
            self.cancel_deferred_spatial_play();
            return;
        }
        let state = self
            .host_analysis
            .source
            .as_ref()
            .map_or(SpatialResume::Unavailable, |source| {
                source.spatial_resume(pending.lease.generation)
            });
        if !already_playing && state == SpatialResume::Warming && Instant::now() < pending.deadline
        {
            return;
        }
        let generation = pending.lease.generation;
        let request_id = pending.request_id;
        self.host_analysis.deferred_play = None;
        if !already_playing
            && state != SpatialResume::Ready
            && self
                .host_analysis
                .lease
                .as_ref()
                .is_some_and(|(lease, _)| lease.generation == generation)
        {
            // Remove the retained mute before the ordinary direct retry.
            self.clear_host_analysis();
        }
        // A transient Playing may belong to an earlier Play, with its Pause
        // still queued/in flight. Submit this explicit latest intent through
        // the provider even then; it can supersede that Pause or no-op safely.
        match self.route_remote(&PlayerCommand::Play, request_id) {
            Some(Ok(())) => {
                if let Some(id) = request_id {
                    self.poll_owned_request(|provider| {
                        provider.owned_play_pause_ready_for_request(id)
                    });
                }
            }
            Some(Err(error)) => {
                if let Some(source) = &mut self.host_analysis.source {
                    source.cancel_spatial_resume();
                }
                self.publish(PlayerEvent::Error { error });
            }
            None => {}
        }
    }
    pub(super) fn revoke_stale_host_analysis(&mut self) {
        if self.host_analysis.lease.is_some() && !self.host_analysis_is_current() {
            self.trace_spatial_next("lease_not_current", false);
            self.clear_host_analysis();
        }
        if let Some(warming) = &self.host_analysis.warming_seek
            && (Instant::now() >= warming.deadline
                || !self
                    .host_analysis
                    .output_clock
                    .as_ref()
                    .is_some_and(|clock| clock.observed.elapsed() <= LEASE_FRESHNESS)
                || !self.owned_playback(&warming.lease).is_some_and(|p| {
                    matches!(p.state, PlaybackState::Playing | PlaybackState::Loading)
                })
                || !self
                    .remote
                    .states
                    .get(&warming.lease.provider)
                    .is_some_and(|state| state.owned_output_control && state.transport_available()))
        {
            self.cancel_host_spatial_next_for("seek_expired_or_projection");
        }
        if let Some(warming) = &self.host_analysis.warming_next
            && (Instant::now() >= warming.deadline
                || warming.manual.as_ref().is_some_and(|manual| {
                    manual.requests.len() > 1
                        && (self
                            .host_analysis
                            .output_clock
                            .as_ref()
                            .is_none_or(|clock| {
                                clock.generation != manual.origin.generation
                                    || clock.device != manual.origin.device
                                    || clock.observed.elapsed() > LEASE_FRESHNESS
                            })
                            || !self
                                .remote
                                .states
                                .get(&warming.lease.provider)
                                .is_some_and(|state| state.owned_output_control))
                })
                || self.remote.active.as_deref() != Some(warming.lease.provider.as_str())
                || !self
                    .remote
                    .states
                    .get(&warming.lease.provider)
                    .is_some_and(|state| {
                        state.owned_output_observed
                            && state.transport_available()
                            && state.playback.as_ref().is_some_and(|p| {
                                (matches!(p.state, PlaybackState::Playing | PlaybackState::Loading)
                                    || warming.paused_origin.as_ref().is_some_and(|paused| {
                                        state.owned_output_control
                                            && p.state == PlaybackState::Paused
                                            && p.position_rate == 0.0
                                            && p.current.as_ref().is_some_and(|item| {
                                                item.id == warming.lease.media_id
                                            })
                                            && paused.observed.elapsed() <= LEASE_FRESHNESS
                                    }))
                                    && p.device.as_ref().is_some_and(|device| {
                                        device.is_active
                                            && !device.is_restricted
                                            && device.id == warming.lease.device_id
                                    })
                            })
                    }))
        {
            self.cancel_host_spatial_next_for("next_expired_or_projection");
        }
    }
    fn renderer_only_pause_proof(&mut self, lease: &HostAnalysisLease, paused: bool) -> bool {
        let Some(authority) = self
            .host_analysis
            .output_clock
            .as_ref()
            .and_then(|clock| self.capture_authority(lease, clock))
        else {
            self.host_analysis.renderer_only_pause = None;
            return false;
        };
        let current = self.host_analysis.output_clock.as_ref().filter(|clock| {
            clock.matches_lease(lease)
                && clock.generation > 0
                && clock.sequence > 0
                && clock.observed.elapsed() <= LEASE_FRESHNESS
                && (clock.moving_playback || clock.stationary_pause)
        });
        let Some((clock, epoch)) = current.and_then(|clock| {
            clock
                .queue_epoch
                .filter(|&epoch| epoch > 0)
                .map(|epoch| (clock, epoch))
        }) else {
            self.host_analysis.renderer_only_pause = None;
            return false;
        };
        let origin = &mut self.host_analysis.renderer_only_pause;
        if origin.as_ref().is_some_and(|origin| {
            !origin.matches(lease, clock, epoch) || origin.authority != authority
        }) {
            *origin = None;
        }
        if clock.moving_playback {
            if lease.playing && clock.full {
                *origin = Some(RendererOnlyPause {
                    lease_generation: lease.generation,
                    runtime_generation: clock.generation,
                    queue_epoch: epoch,
                    playing_sequence: clock.sequence,
                    playing_observed: clock.observed,
                    confirmed: false,
                    authority,
                });
            } else if origin.as_ref().is_some_and(|origin| origin.confirmed) {
                *origin = None;
            }
            return false;
        }
        origin.as_mut().is_some_and(|origin| {
            if paused
                && clock.sequence > origin.playing_sequence
                && (origin.confirmed
                    || clock.full && origin.playing_observed.elapsed() <= LEASE_FRESHNESS)
            {
                origin.confirmed = true;
                true
            } else {
                false
            }
        })
    }

    fn capture_authority(
        &self,
        lease: &HostAnalysisLease,
        clock: &SpatialOutputClock,
    ) -> Option<CaptureAuthority> {
        if !lease.spatial_stereo
            || !clock.matches_lease(lease)
            || clock.observed.elapsed() > LEASE_FRESHNESS
        {
            return None;
        }
        let state = self.remote.states.get(&lease.provider)?;
        if !state.owned_output_observed
            || !state.transport_available()
            || state.authentication_required
        {
            return None;
        }
        let retains_witness = self
            .host_analysis
            .renderer_only_pause
            .as_ref()
            .is_some_and(|origin| matches!(origin.authority, CaptureAuthority::Witness(_)));
        if state.owned_output_control && !retains_witness {
            return Some(CaptureAuthority::Control);
        }
        let witness = self
            .remote
            .providers
            .get(&lease.provider)
            .and_then(|provider| provider.owned_capture_witness());
        if let Some(witness) =
            witness.filter(|witness| self.capture_witness_matches(lease, clock, witness))
        {
            // A later HTTP/control binding does not replace a still-valid capture identity.
            // Revocation or reissue of that witness still changes authority and clears its pause.
            Some(CaptureAuthority::Witness(witness.id))
        } else {
            state
                .owned_output_control
                .then_some(CaptureAuthority::Control)
        }
    }

    fn capture_witness_matches(
        &self,
        lease: &HostAnalysisLease,
        clock: &SpatialOutputClock,
        witness: &OwnedCaptureWitness,
    ) -> bool {
        witness.id > 0
            && witness.generation > 0
            && witness.queue_epoch > 0
            && witness.playing_sequence > 0
            && witness.playing_control_uptime_ms > 0
            && witness.generation == clock.generation
            && witness.device_id == lease.device_id
            && witness.media_id == lease.media_id
            && Some(witness.queue_epoch) == clock.queue_epoch
            && clock.sequence >= witness.playing_sequence
            && clock
                .control_uptime_ms
                .is_some_and(|uptime| uptime >= witness.playing_control_uptime_ms)
            && (clock.context.as_deref() == Some(witness.context_uri.as_str())
                || clock.context.is_none()
                    && !clock.full
                    && (clock.stationary_pause || clock.incomplete_zero_playback))
    }

    fn waiting_for_playing_host_lease(&self, lease: &HostAnalysisLease) -> bool {
        let authority = self
            .host_analysis
            .output_clock
            .as_ref()
            .and_then(|clock| self.capture_authority(lease, clock));
        if !lease.spatial_stereo
            || lease.playing
            || self.host_analysis.pause_pending_until.is_some()
            || authority.is_none()
        {
            return false;
        }
        self.host_analysis
            .output_clock
            .as_ref()
            .is_some_and(|clock| {
                clock.matches_lease(lease)
                    && clock.observed.elapsed() <= LEASE_FRESHNESS
                    && clock.queue_epoch.is_some_and(|epoch| {
                        epoch > 0
                            && self.host_analysis.renderer_only_pause.as_ref().is_some_and(
                                |origin| {
                                    origin.confirmed
                                        && Some(origin.authority) == authority
                                        && origin.matches(lease, clock, epoch)
                                },
                            )
                    })
                    && (clock.full
                        && clock.moving_playback
                        && self.owned_playback_is_playing(lease)
                        || (clock.incomplete_zero_playback || clock.full && clock.moving_playback)
                            && self
                                .host_analysis
                                .resume_clock_gap
                                .as_ref()
                                .is_some_and(|gap| {
                                    Instant::now() < gap.deadline
                                        && gap.lease.same_output(lease)
                                        && gap.paused.generation == clock.generation
                                        && gap.paused.queue_epoch == clock.queue_epoch
                                        && gap.paused.context == clock.context
                                        && clock.sequence > gap.paused.sequence
                                })
                            && self.owned_playback(lease).is_some_and(|playback| {
                                matches!(
                                    playback.state,
                                    PlaybackState::Paused | PlaybackState::Playing
                                ) && playback.position_rate == 0.0
                            }))
            })
    }

    pub(super) fn poll_host_analysis(&mut self) -> Option<(MediaId, AnalysisFrame)> {
        self.revoke_stale_host_analysis();
        let mut lease = self.host_analysis.lease.as_ref()?.0.clone();
        let waiting_for_lease = self.waiting_for_playing_host_lease(&lease);
        if !lease.playing
            && self
                .host_analysis
                .output_clock
                .as_ref()
                .is_some_and(|clock| clock.incomplete_zero_playback || clock.moving_playback)
            && self
                .host_analysis
                .renderer_only_pause
                .as_ref()
                .is_some_and(|origin| origin.confirmed)
        {
            self.trace_spatial_resume_clock_gap(if waiting_for_lease {
                "holding"
            } else {
                "hold_rejected"
            });
        }
        if waiting_for_lease {
            // Raw transport arrives before the host's matching resume lease.
            // Preserve only the previously confirmed stopped snapshot until
            // that lease arrives; its existing freshness deadline still applies.
            return None;
        }
        let paused_rate = self
            .owned_playback(&lease)
            .filter(|playback| playback.state == PlaybackState::Paused)
            .map(|playback| playback.position_rate);
        if paused_rate.is_some() {
            self.host_analysis.pause_pending_until = None;
        }
        lease.playing = self.host_analysis_is_playing(&lease);
        let confirmed_pause = (lease.spatial_stereo && !lease.playing && paused_rate == Some(0.0))
            .then_some(lease.generation);
        let renderer_only_pause = self
            .renderer_only_pause_proof(&lease, confirmed_pause.is_some())
            .then_some(lease.generation);
        let source = self.host_analysis.source.as_mut()?;
        source.set_lease_with_pause_proofs(
            Some(lease.clone()),
            confirmed_pause,
            renderer_only_pause,
        );
        if !lease.visuals || !lease.playing {
            return None;
        }
        source
            .analysis()
            .filter(|frame| frame.generation == lease.generation && frame.is_valid())
            .map(|frame| (lease.media_id, frame))
    }
}
/// Conservative invalidation includes authentication and all remote transport
/// intents. Volume and catalogue operations do not interrupt capture.
pub(super) fn invalidates_lease(command: &PlayerCommand) -> bool {
    if let PlayerCommand::Remote {
        action: RemoteAction::Control { control },
        ..
    } = command
    {
        return !matches!(control, RemoteControl::Volume { .. });
    }
    matches!(
        command,
        PlayerCommand::Play
            | PlayerCommand::Pause
            | PlayerCommand::Stop
            | PlayerCommand::Next
            | PlayerCommand::Previous
            | PlayerCommand::SkipPrevious
            | PlayerCommand::Seek { .. }
            | PlayerCommand::OpenFile { .. }
            | PlayerCommand::ReplaceQueue { .. }
            | PlayerCommand::PlayLibrary { .. }
            | PlayerCommand::JumpQueue { .. }
            | PlayerCommand::ClearQueue
            | PlayerCommand::Remote {
                action: RemoteAction::Activate
                    | RemoteAction::Disconnect
                    | RemoteAction::Credential { .. },
                ..
            }
    )
}

pub(super) fn independent_remote_queue_edit(command: &PlayerCommand) -> bool {
    matches!(
        command,
        PlayerCommand::Remote {
            action: RemoteAction::Control {
                control: RemoteControl::Enqueue { .. }
                    | RemoteControl::Shuffle { .. }
                    | RemoteControl::Repeat { .. },
            },
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Slot {
        lease: Option<HostAnalysisLease>,
        confirmed_pause: Option<u64>,
        confirmed_renderer_only_pause: Option<u64>,
        pause_publications: usize,
        frame: Option<AnalysisFrame>,
        resume: SpatialResume,
        next_warm: Option<u64>,
        natural_next_warm: Option<u64>,
        seek_warm: Option<u64>,
        reanchor: Option<(u64, u64)>,
        resume_invalidations: Vec<u64>,
        spatial_status: Option<HostSpatialStatus>,
    }
    struct Source(Arc<Mutex<Slot>>);
    impl HostOutputAnalysis for Source {
        fn set_lease(&mut self, lease: Option<HostAnalysisLease>) {
            let mut slot = self.0.lock().unwrap();
            if lease.is_none() {
                slot.resume = SpatialResume::Unavailable;
            }
            slot.lease = lease;
        }
        fn set_lease_with_pause_proofs(
            &mut self,
            lease: Option<HostAnalysisLease>,
            confirmed_pause: Option<u64>,
            renderer_only_pause: Option<u64>,
        ) {
            let mut slot = self.0.lock().unwrap();
            if let Some(generation) = confirmed_pause {
                assert!(lease.as_ref().is_some_and(|lease| {
                    lease.generation == generation && lease.spatial_stereo && !lease.playing
                }));
            }
            if renderer_only_pause.is_some() {
                assert_eq!(renderer_only_pause, confirmed_pause);
            }
            if lease.is_none() {
                slot.resume = SpatialResume::Unavailable;
            }
            slot.lease = lease;
            slot.confirmed_pause = confirmed_pause;
            slot.confirmed_renderer_only_pause = renderer_only_pause;
            slot.pause_publications += 1;
        }
        fn set_confirmed_spatial_pause(&mut self, generation: Option<u64>) {
            let mut slot = self.0.lock().unwrap();
            if let Some(generation) = generation {
                assert!(slot.lease.as_ref().is_some_and(|lease| {
                    lease.generation == generation && lease.spatial_stereo && !lease.playing
                }));
            }
            slot.confirmed_pause = generation;
        }
        fn set_confirmed_renderer_only_pause(&mut self, generation: Option<u64>) {
            let mut slot = self.0.lock().unwrap();
            if generation.is_some() {
                assert_eq!(slot.confirmed_pause, generation);
            }
            slot.confirmed_renderer_only_pause = generation;
        }
        fn analysis(&self) -> Option<AnalysisFrame> {
            self.0.lock().unwrap().frame
        }
        fn revoke_for_spatial_next(&mut self, generation: u64) {
            let mut slot = self.0.lock().unwrap();
            let lease = slot.lease.take().unwrap();
            assert_eq!(lease.generation, generation);
            if !lease.playing {
                assert_eq!(slot.confirmed_pause, Some(generation));
            }
            slot.next_warm = Some(generation);
        }
        fn revoke_for_natural_spatial_next(&mut self, generation: u64) {
            let mut slot = self.0.lock().unwrap();
            let lease = slot.lease.take().unwrap();
            assert_eq!(lease.generation, generation);
            assert!(lease.playing && lease.spatial_stereo);
            slot.natural_next_warm = Some(generation);
        }
        fn revoke_for_spatial_seek(&mut self, generation: u64) {
            let mut slot = self.0.lock().unwrap();
            let lease = slot.lease.take().unwrap();
            assert_eq!(lease.generation, generation);
            assert!(lease.playing && lease.spatial_stereo);
            slot.seek_warm = Some(generation);
        }
        fn revoke_for_spatial_reanchor(
            &mut self,
            generation: u64,
            replacement: u64,
            deadline: Instant,
        ) {
            let mut slot = self.0.lock().unwrap();
            let lease = slot.lease.take().unwrap();
            assert_eq!(lease.generation, generation);
            assert!(lease.playing && lease.spatial_stereo);
            assert!(deadline > Instant::now());
            slot.reanchor = Some((generation, replacement));
        }
        fn cancel_spatial_next(&mut self) {
            let mut slot = self.0.lock().unwrap();
            slot.next_warm = None;
            slot.natural_next_warm = None;
            slot.seek_warm = None;
            slot.reanchor = None;
        }
        fn prepare_spatial_resume(&mut self, generation: u64) -> bool {
            self.spatial_resume(generation) != SpatialResume::Unavailable
        }
        fn spatial_resume(&self, generation: u64) -> SpatialResume {
            let slot = self.0.lock().unwrap();
            if slot
                .lease
                .as_ref()
                .is_some_and(|lease| lease.generation == generation)
            {
                slot.resume
            } else {
                SpatialResume::Unavailable
            }
        }
        fn cancel_spatial_resume(&mut self) {
            self.0.lock().unwrap().resume = SpatialResume::Unavailable;
        }
        fn invalidate_spatial_resume_after_seek(&mut self, generation: u64) {
            self.0.lock().unwrap().resume_invalidations.push(generation);
        }
        fn status(&self) -> HostAnalysisStatus {
            if self.0.lock().unwrap().frame.is_some() {
                HostAnalysisStatus::Running
            } else {
                HostAnalysisStatus::TapUnavailable
            }
        }
        fn spatial_status(&self) -> HostSpatialStatus {
            let slot = self.0.lock().unwrap();
            if slot
                .lease
                .as_ref()
                .is_some_and(|lease| lease.spatial_stereo)
            {
                slot.spatial_status.unwrap_or(HostSpatialStatus::Active)
            } else {
                HostSpatialStatus::Idle
            }
        }
    }
    fn setup() -> (PlaybackCoordinator, Arc<Mutex<Slot>>, HostAnalysisLease) {
        let mut core = PlaybackCoordinator::default();
        let slot = Arc::new(Mutex::new(Slot::default()));
        core.register_host_analysis(Source(slot.clone()));
        let mut item = provider_mock::demo_tracks().remove(0);
        item.id = MediaId::parse("fixture:track:owned").unwrap();
        item.capabilities = Capabilities::REMOTE;
        let lease = HostAnalysisLease {
            generation: 1,
            process_id: 12345,
            provider: "fixture".into(),
            device_id: "owned".into(),
            media_id: item.id.clone(),
            spatial_stereo: false,
            visuals: true,
            playing: true,
        };
        core.remote.active = Some("fixture".into());
        core.remote.states.insert(
            "fixture".into(),
            RemoteState {
                owned_output_observed: true,
                owned_output_control: false,
                control_result: None,
                confirmed_same_media_next_request_id: None,
                observation_id: 1,
                provider: "fixture".into(),
                connection: ProviderConnectionState::Connected,
                authentication_required: false,
                issue: None,
                retry_after_seconds: None,
                devices: vec![],
                playback: Some(RemotePlayback {
                    enhancement: None,
                    context_uri: None,
                    state: PlaybackState::Playing,
                    position_ms: 0,
                    position_rate: 1.0,
                    volume: 0.4,
                    current: Some(item),
                    upcoming: vec![],
                    device_queue: None,
                    shuffle: false,
                    repeat: RepeatMode::Off,
                    capabilities: Capabilities::REMOTE,
                    transport: TransportControls::default(),
                    device: Some(PlaybackDevice {
                        id: "owned".into(),
                        name: "Original owned fixture".into(),
                        device_type: "Computer".into(),
                        volume: Some(0.4),
                        is_active: true,
                        is_restricted: false,
                    }),
                }),
            },
        );
        slot.lock().unwrap().frame = Some(AnalysisFrame {
            generation: 1,
            sample_rate: 48000,
            window_frames: 4096,
            hop_frames: 960,
            window_start_frame: 704,
            window_end_frame: 4800,
            dropped_frames: 0,
            spectrum: [0.01; 32],
            features: AudioFeatures {
                playback_position_ms: 100,
                rms: 0.02,
                bass: 0.02,
                ..Default::default()
            },
        });
        (core, slot, lease)
    }

    #[test]
    fn relay_lease_stays_current_without_visuals_and_revokes_on_transport() {
        let (mut core, slot, mut lease) = setup();
        lease.spatial_stereo = true;
        lease.visuals = false;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(core.poll_host_analysis().is_none());
        assert_eq!(
            core.snapshot().host_spatial_status,
            Some(HostSpatialStatus::Active)
        );
        assert!(!core.snapshot().capabilities.pcm_analysis);
        core.send(PlayerCommand::HostAnalysis {
            lease: Some(lease.clone()),
        })
        .unwrap();
        assert!(core.events.iter().any(|event| matches!(event,
            PlayerEvent::Snapshot { state } if state.host_spatial_status == Some(HostSpatialStatus::Active)
                && !state.capabilities.pcm_analysis)));

        for command in [
            PlayerCommand::SetVolume { volume: 0.3 },
            PlayerCommand::Remote {
                provider: lease.provider.clone(),
                action: RemoteAction::Control {
                    control: RemoteControl::Volume { volume: 0.3 },
                },
            },
        ] {
            let _ = core.send(command);
            assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        }

        let mut forged = lease.clone();
        forged.process_id += 1;
        core.update_host_analysis(Some(forged)).unwrap();
        assert!(core.poll_host_analysis().is_none());
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));

        lease.visuals = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(core.poll_host_analysis().is_some());
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert_eq!(
            core.snapshot().host_spatial_status,
            Some(HostSpatialStatus::Active)
        );

        let _ = core.send(PlayerCommand::Seek { position_ms: 200 });
        core.update_host_analysis(Some(lease)).unwrap();
        assert!(core.poll_host_analysis().is_none());
        assert!(core.snapshot().host_spatial_status.is_none());
        assert!(slot.lock().unwrap().lease.is_none());
    }
    #[test]
    fn spatial_pause_mutes_before_confirmation_and_preserves_the_acquired_route() {
        let (mut core, slot, mut lease) = setup();
        lease.spatial_stereo = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(core.poll_host_analysis().is_some());
        assert_eq!(
            core.preserve_host_spatial_transport(&PlayerCommand::Pause),
            Some(PlaybackState::Paused)
        );
        assert_eq!(
            slot.lock().unwrap().lease.as_ref().map(|l| l.playing),
            Some(false)
        );
        assert_eq!(slot.lock().unwrap().confirmed_pause, None);
        // An in-flight playing heartbeat must not cancel the pending pause.
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(core.poll_host_analysis().is_none());
        assert!(!core.host_visuals_are_current());
        assert_eq!(
            slot.lock().unwrap().lease.as_ref().map(|l| l.playing),
            Some(false)
        );
        let fresh_frame = slot.lock().unwrap().frame.take().unwrap();

        let set_state = |core: &mut PlaybackCoordinator, state, rate| {
            let playback = core
                .remote
                .states
                .get_mut("fixture")
                .unwrap()
                .playback
                .as_mut()
                .unwrap();
            playback.state = state;
            playback.position_rate = rate;
        };
        lease.playing = false;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        set_state(&mut core, PlaybackState::Paused, 1.0);
        assert!(core.poll_host_analysis().is_none());
        assert_eq!(slot.lock().unwrap().confirmed_pause, None);
        set_state(&mut core, PlaybackState::Paused, 0.0);
        assert!(core.poll_host_analysis().is_none());
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert_eq!(slot.lock().unwrap().confirmed_pause, Some(lease.generation));
        assert_eq!(
            core.preserve_host_spatial_transport(&PlayerCommand::Play),
            Some(PlaybackState::Playing)
        );
        set_state(&mut core, PlaybackState::Loading, 0.0);
        assert!(core.poll_host_analysis().is_none());
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert_eq!(slot.lock().unwrap().confirmed_pause, None);

        lease.playing = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(
            core.poll_host_analysis().is_none(),
            "Await the owned playing state"
        );
        set_state(&mut core, PlaybackState::Playing, 0.0);
        assert!(core.poll_host_analysis().is_none());
        assert_eq!(
            slot.lock().unwrap().lease.as_ref().map(|l| l.playing),
            Some(false),
            "A zero-rate playing notification cannot unmute the relay"
        );
        set_state(&mut core, PlaybackState::Playing, 1.0);
        assert!(core.poll_host_analysis().is_none());
        slot.lock().unwrap().frame = Some(fresh_frame);
        assert!(core.poll_host_analysis().is_some());
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert_eq!(core.host_analysis_revoked_through(), None);

        let _ = core.send(PlayerCommand::Seek { position_ms: 200 });
        assert!(slot.lock().unwrap().lease.is_none());
        assert_eq!(core.host_analysis_revoked_through(), Some(lease.generation));
    }

    #[test]
    fn addressed_pause_preserves_only_its_exact_existing_spatial_output() {
        for (provider, device, keep) in [
            ("fixture", "owned", true),
            ("fixture", "external", false),
            ("other", "owned", false),
        ] {
            let (mut core, slot, mut lease) = setup();
            let calls = Arc::new(ResumeCalls::default());
            core.register_remote(ResumeProvider(calls.clone()));
            lease.spatial_stereo = true;
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_host_analysis();
            // The confirmed selection can precede ordinary capability projection.
            let playback = core
                .remote
                .states
                .get_mut("fixture")
                .unwrap()
                .playback
                .as_mut()
                .unwrap();
            playback.capabilities.pause = false;
            playback.current.as_mut().unwrap().capabilities.pause = false;
            let result = core.send(PlayerCommand::Remote {
                provider: provider.into(),
                action: RemoteAction::Control {
                    control: RemoteControl::PauseOnDevice {
                        device_id: device.into(),
                    },
                },
            });
            if keep {
                assert!(result.is_ok());
                assert_eq!(
                    core.host_analysis.lease.as_ref().unwrap().0.generation,
                    lease.generation
                );
                assert!(core.host_analysis.pause_pending_until.is_some());
                let source = slot.lock().unwrap();
                assert!(
                    source
                        .lease
                        .as_ref()
                        .is_some_and(|current| { current.same_output(&lease) && !current.playing })
                );
                assert_eq!(source.confirmed_pause, None);
                assert!(
                    matches!(calls.actions.lock().unwrap().as_slice(), [RemoteAction::Control {
                    control: RemoteControl::PauseOnDevice { device_id },
                }] if device_id == device)
                );
            } else {
                assert!(core.host_analysis.lease.is_none(), "{provider}/{device}");
                assert!(slot.lock().unwrap().lease.is_none(), "{provider}/{device}");
            }
        }

        let (mut core, slot, _) = setup();
        core.register_remote(ResumeProvider(Arc::new(ResumeCalls::default())));
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Control {
                control: RemoteControl::PauseOnDevice {
                    device_id: "owned".into(),
                },
            },
        })
        .unwrap();
        assert!(core.host_analysis.lease.is_none());
        assert!(slot.lock().unwrap().lease.is_none());
    }

    #[test]
    fn paused_output_cannot_acquire_a_spatial_route() {
        let (mut core, slot, mut lease) = setup();
        let playback = core
            .remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.state = PlaybackState::Paused;
        lease.spatial_stereo = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(!core.host_analysis_is_current());
        lease.generation += 1;
        lease.playing = false;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(core.poll_host_analysis().is_none());
        assert!(slot.lock().unwrap().lease.is_none());
        assert_eq!(slot.lock().unwrap().confirmed_pause, None);
        assert_eq!(core.host_analysis_revoked_through(), Some(lease.generation));
    }

    #[test]
    fn only_current_owned_lease_can_add_analysis_without_remote_eq() {
        let (mut core, slot, lease) = setup();
        core.poll_analysis();
        assert!(core.last_analysis.is_none());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert!(core.last_analysis.is_some());
        assert_eq!(slot.lock().unwrap().lease, Some(lease.clone()));
        let snapshot = core.snapshot();
        assert!(snapshot.capabilities.pcm_analysis);
        assert!(!snapshot.capabilities.equalizer);
        assert!(!snapshot.queue.current.unwrap().capabilities.pcm_analysis);
        assert_eq!(
            snapshot.host_analysis_status,
            Some(HostAnalysisStatus::Running)
        );
        slot.lock().unwrap().frame = None;
        core.poll_analysis();
        let unavailable = core.snapshot();
        assert!(!unavailable.capabilities.pcm_analysis);
        assert_eq!(
            unavailable.host_analysis_status,
            Some(HostAnalysisStatus::TapUnavailable)
        );
        assert_eq!(
            unavailable.state,
            PlaybackState::Playing,
            "Capture failure cannot stop transport"
        );
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .device
            .as_mut()
            .unwrap()
            .id = "external".into();
        core.poll_analysis();
        assert!(core.last_analysis.is_none());
        assert!(slot.lock().unwrap().lease.is_none());
        assert!(!core.snapshot().capabilities.pcm_analysis);
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .device
            .as_mut()
            .unwrap()
            .id = "owned".into();
        core.update_host_analysis(Some(lease)).unwrap();
        core.poll_analysis();
        assert!(
            core.last_analysis.is_none(),
            "revoked generation cannot revive on late heartbeat"
        );
    }
    #[test]
    fn a_bound_clock_keeps_the_lease_during_an_http_outage_and_loss_revokes_it() {
        let (mut core, slot, mut lease) = setup();
        assert_eq!(core.snapshot().host_analysis_revoked_through, None);
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();

        for connection in [
            ProviderConnectionState::Unavailable,
            ProviderConnectionState::Connected,
        ] {
            core.remote
                .states
                .get_mut(&lease.provider)
                .unwrap()
                .connection = connection;
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_analysis();
            let snapshot = core.snapshot();
            assert_eq!(snapshot.provider_connection, connection);
            assert!(snapshot.capabilities.pcm_analysis);
            assert_eq!(
                snapshot.host_analysis_status,
                Some(HostAnalysisStatus::Running)
            );
            assert_eq!(snapshot.host_analysis_revoked_through, None);
            assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        }

        let state = core.remote.states.get_mut(&lease.provider).unwrap();
        state.connection = ProviderConnectionState::Unavailable;
        state.owned_output_observed = false;
        core.publish_snapshot();
        let published = std::iter::from_fn(|| core.pop_event())
            .find_map(|event| match event {
                PlayerEvent::Snapshot { state } => Some(state),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            published.host_analysis_revoked_through,
            Some(lease.generation)
        );
        assert!(published.host_analysis_status.is_none());
        assert!(!published.capabilities.pcm_analysis);
        assert!(slot.lock().unwrap().lease.is_none());
        assert!(core.last_analysis.is_none());

        // The native host can miss the intermediate state when snapshots coalesce.
        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .owned_output_observed = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert!(core.last_analysis.is_none());
        assert!(slot.lock().unwrap().lease.is_none());
        lease.generation += 1;
        slot.lock().unwrap().frame.as_mut().unwrap().generation = lease.generation;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert!(core.snapshot().capabilities.pcm_analysis);
        assert_eq!(
            core.snapshot().host_analysis_revoked_through,
            Some(lease.generation - 1)
        );
    }

    #[test]
    fn host_capture_requires_an_owned_observation_and_an_active_unrestricted_device() {
        for (connection, owned, active, restricted) in [
            (ProviderConnectionState::Connected, false, true, false),
            (ProviderConnectionState::Connected, true, false, false),
            (ProviderConnectionState::Connected, true, true, true),
            (ProviderConnectionState::Connecting, true, true, false),
        ] {
            let (mut core, slot, lease) = setup();
            let state = core.remote.states.get_mut(&lease.provider).unwrap();
            state.connection = connection;
            state.owned_output_observed = owned;
            let device = state.playback.as_mut().unwrap().device.as_mut().unwrap();
            device.is_active = active;
            device.is_restricted = restricted;
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_analysis();
            assert!(slot.lock().unwrap().lease.is_none());
            assert!(core.last_analysis.is_none());
            assert_eq!(
                core.snapshot().host_analysis_revoked_through,
                Some(lease.generation)
            );
        }
    }

    #[test]
    fn invalidation_rejects_old_epoch_and_track_frames_and_expired_auth() {
        let (mut core, slot, mut lease) = setup();
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert!(invalidates_lease(&PlayerCommand::Seek {
            position_ms: 4000
        }));
        let _ = core.send(PlayerCommand::Seek { position_ms: 4000 });
        core.poll_analysis();
        assert!(core.last_analysis.is_none());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(!core.host_analysis_is_current());
        lease.generation = 2;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert!(
            core.last_analysis.is_none(),
            "old frame cannot cross seek epoch"
        );
        slot.lock().unwrap().frame.as_mut().unwrap().generation = 2;
        core.poll_analysis();
        assert!(core.last_analysis.is_some());
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .current
            .as_mut()
            .unwrap()
            .id = MediaId::parse("fixture:track:other").unwrap();
        core.poll_analysis();
        assert!(core.last_analysis.is_none());
        lease.media_id = MediaId::parse("fixture:track:other").unwrap();
        lease.generation = 3;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.host_analysis.lease.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(1);
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert_eq!(core.host_analysis_revoked_through(), Some(lease.generation));
        core.poll_analysis();
        assert!(slot.lock().unwrap().lease.is_none());
        lease.generation = 4;
        core.update_host_analysis(Some(lease)).unwrap();
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .authentication_required = true;
        core.poll_analysis();
        assert!(!core.host_analysis_is_current());
        assert!(core.last_analysis.is_none());
    }
    #[test]
    fn pause_disconnect_source_and_credentials_invalidate_but_catalogue_does_not() {
        for command in [
            PlayerCommand::Pause,
            PlayerCommand::Next,
            PlayerCommand::OpenFile {
                path: "/original.wav".into(),
            },
            PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::Disconnect,
            },
            PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::Credential {
                    access_token: "original-fixture".into(),
                },
            },
        ] {
            assert!(invalidates_lease(&command));
        }
        assert!(!invalidates_lease(&PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::OutputClock { clock: None }
        }));
        let (mut core, slot, lease) = setup();
        core.update_host_analysis(Some(lease)).unwrap();
        core.poll_analysis();
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .state = PlaybackState::Paused;
        core.poll_analysis();
        assert!(core.last_analysis.is_none());
        assert!(slot.lock().unwrap().lease.is_none());
    }

    #[derive(Default)]
    struct ResumeCalls {
        actions: Mutex<Vec<RemoteAction>>,
        pending_start: AtomicBool,
        play_request_ids: Mutex<Vec<u64>>,
        polled_owned_play: AtomicBool,
        capture_witness: Mutex<Option<OwnedCaptureWitness>>,
        relative_clock: Mutex<Option<RemoteOutputClock>>,
        relative_updates: Mutex<VecDeque<RemoteUpdate>>,
        relative_requests: Mutex<Vec<(u64, RemoteOutputControl)>>,
        relative_serial: Mutex<u64>,
        hold_relative: AtomicBool,
        reject_relative: AtomicBool,
    }

    impl ResumeCalls {
        fn dispatch_relative(
            &self,
            request_id: u64,
            control: RemoteOutputControl,
        ) -> Result<(), PlayerError> {
            self.relative_requests
                .lock()
                .unwrap()
                .push((request_id, control.clone()));
            if self.reject_relative.load(Ordering::Relaxed) {
                return Err(PlayerError::invalid("Original relative rejection"));
            }
            if self.hold_relative.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Some(clock) = self.relative_clock.lock().unwrap().clone() else {
                return Ok(());
            };
            let mut serial = self.relative_serial.lock().unwrap();
            *serial += 1;
            self.relative_updates
                .lock()
                .unwrap()
                .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                    id: *serial,
                    request_id: Some(request_id),
                    expected_previous_media_id: None,
                    required_queue_epoch: None,
                    observed_queue_epoch: clock.queue_epoch,
                    generation: clock.generation,
                    sequence: clock.sequence,
                    device_id: clock.device_id.clone(),
                    media_id: clock.media_id.clone().unwrap(),
                    expires_at_uptime_ms: clock.control_uptime_ms.unwrap() + 1500,
                    control,
                }));
            Ok(())
        }
    }

    struct ResumeProvider(Arc<ResumeCalls>);

    impl RemoteProvider for ResumeProvider {
        fn id(&self) -> &str {
            "fixture"
        }
        fn owned_capture_witness(&self) -> Option<OwnedCaptureWitness> {
            self.0.capture_witness.lock().unwrap().clone()
        }
        fn submit(&mut self, action: RemoteAction) -> Result<(), PlayerError> {
            self.0.actions.lock().unwrap().push(action);
            Ok(())
        }
        fn submit_play_pause_with_request_id(
            &mut self,
            play: bool,
            request_id: u64,
        ) -> Result<(), PlayerError> {
            if play {
                self.0.play_request_ids.lock().unwrap().push(request_id);
            }
            self.submit(RemoteAction::Control {
                control: if play {
                    RemoteControl::Play
                } else {
                    RemoteControl::Pause
                },
            })
        }
        fn submit_next_with_request_id(&mut self, request_id: u64) -> Result<(), PlayerError> {
            if self.0.relative_clock.lock().unwrap().is_none() {
                return self.submit(RemoteAction::Control {
                    control: RemoteControl::Next,
                });
            }
            self.0
                .dispatch_relative(request_id, RemoteOutputControl::Next)
        }
        fn submit_previous_with_request_id(&mut self, request_id: u64) -> Result<(), PlayerError> {
            if self.0.relative_clock.lock().unwrap().is_none() {
                return self.submit(RemoteAction::Control {
                    control: RemoteControl::Previous,
                });
            }
            self.0
                .dispatch_relative(request_id, RemoteOutputControl::Previous)
        }
        fn owned_next_ready_for_request(&self, request_id: u64) -> bool {
            self.0
                .relative_updates
                .lock()
                .unwrap()
                .iter()
                .any(|update| {
                    matches!(update,
                RemoteUpdate::OutputCommand(command) if command.request_id == Some(request_id))
                })
        }
        fn owned_handoff_ready(&self) -> bool {
            !self.0.relative_updates.lock().unwrap().is_empty()
        }
        fn owned_play_pause_ready_for_request(&self, request_id: u64) -> bool {
            self.0.play_request_ids.lock().unwrap().last() == Some(&request_id)
        }
        fn poll(&mut self) -> Option<RemoteUpdate> {
            if !self.0.play_request_ids.lock().unwrap().is_empty() {
                self.0.polled_owned_play.store(true, Ordering::Relaxed);
            }
            self.0.relative_updates.lock().unwrap().pop_front()
        }
        fn has_pending_output_start(&self) -> bool {
            self.0.pending_start.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn explicit_owned_next_releases_capture_and_sends_transport_without_waiting() {
        for interruption in [None, Some(PlayerCommand::Pause), Some(PlayerCommand::Next)] {
            let (mut core, slot, mut lease) = setup();
            let calls = Arc::new(ResumeCalls::default());
            core.register_remote(ResumeProvider(calls.clone()));
            let state = core.remote.states.get_mut(&lease.provider).unwrap();
            state.owned_output_control = true;
            state.playback.as_mut().unwrap().transport.next = true;
            lease.spatial_stereo = true;
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_analysis();

            core.send(PlayerCommand::Next).unwrap();
            assert!(matches!(
                calls.actions.lock().unwrap().as_slice(),
                [RemoteAction::Control {
                    control: RemoteControl::Next
                }]
            ));
            assert_eq!(slot.lock().unwrap().next_warm, Some(lease.generation));
            assert!(!core.host_analysis_is_current());
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_analysis();
            assert!(slot.lock().unwrap().lease.is_none());

            if let Some(command) = interruption {
                core.send(command).unwrap();
                assert_eq!(slot.lock().unwrap().next_warm, None);
                assert_eq!(calls.actions.lock().unwrap().len(), 2);
            } else {
                lease.generation += 1;
                lease.media_id = MediaId::parse("fixture:track:new").unwrap();
                core.remote
                    .states
                    .get_mut(&lease.provider)
                    .unwrap()
                    .playback
                    .as_mut()
                    .unwrap()
                    .current
                    .as_mut()
                    .unwrap()
                    .id = lease.media_id.clone();
                core.update_host_analysis(Some(lease.clone())).unwrap();
                core.poll_analysis();
                assert_eq!(slot.lock().unwrap().lease, Some(lease));
                assert_eq!(slot.lock().unwrap().next_warm, Some(1));
            }
        }
    }

    fn manual_relative_fixture() -> (
        PlaybackCoordinator,
        Arc<Mutex<Slot>>,
        Arc<ResumeCalls>,
        HostAnalysisLease,
        RemoteOutputClock,
    ) {
        let (mut core, slot, mut lease) = setup();
        let calls = Arc::new(ResumeCalls::default());
        core.register_remote(ResumeProvider(calls.clone()));
        lease.spatial_stereo = true;
        let state = core.remote.states.get_mut("fixture").unwrap();
        state.owned_output_control = true;
        let playback = state.playback.as_mut().unwrap();
        playback.transport.next = true;
        playback.transport.previous = true;
        playback.context_uri = Some("fixture:album:relative".into());
        let mut clock = paused_output_clock(&lease);
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        clock.position_ms = 1000;
        clock.queue_epoch = Some(7);
        clock.actions = Some(RemoteOutputActions {
            next: true,
            previous: true,
            pause: true,
            ..Default::default()
        });
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Original relative fixture".into(),
            artists: vec!["Fixture".into()],
            album: None,
            duration_ms: 80_000,
            artwork: None,
            context_uri: playback.context_uri.clone(),
        });
        core.update_host_analysis(Some(lease.clone())).unwrap();
        relative_frame(&mut core, &calls, clock.clone());
        core.poll_analysis();
        (core, slot, calls, lease, clock)
    }

    fn relative_frame(
        core: &mut PlaybackCoordinator,
        calls: &ResumeCalls,
        clock: RemoteOutputClock,
    ) {
        let state = core.remote.states.get_mut("fixture").unwrap();
        state.observation_id += 1;
        let playback = state.playback.as_mut().unwrap();
        playback.state = clock.state;
        playback.position_rate = clock.rate;
        playback.position_ms = clock.position_ms;
        playback.context_uri = clock
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.context_uri.clone());
        playback.current.as_mut().unwrap().id = clock.media_id.clone().unwrap();
        *calls.relative_clock.lock().unwrap() = Some(clock.clone());
        observe_spatial_next_clock(core, clock);
    }

    fn relative_ack(core: &mut PlaybackCoordinator, id: u64, accepted: bool) {
        core.check_host_spatial_clock(&PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::OutputCommandResult {
                result: RemoteOutputCommandResult {
                    id,
                    generation: 2,
                    accepted,
                },
            },
        });
    }

    #[test]
    fn manual_relative_burst_retains_only_muted_standby_until_the_latest_dispatched_occurrence() {
        let (mut core, slot, calls, mut lease, mut clock) = manual_relative_fixture();
        core.send_request(CommandRequest {
            request_id: 41,
            command: PlayerCommand::Next,
        });
        let deadline = core.host_analysis.warming_next.as_ref().unwrap().deadline;
        calls.hold_relative.store(true, Ordering::Relaxed);
        for (request_id, command) in [(42, PlayerCommand::Next), (43, PlayerCommand::SkipPrevious)]
        {
            core.send_request(CommandRequest {
                request_id,
                command,
            });
            assert_eq!(
                core.host_analysis.warming_next.as_ref().unwrap().deadline,
                deadline
            );
            assert!(core.host_analysis.lease.is_none() && slot.lock().unwrap().lease.is_none());
            assert_eq!(slot.lock().unwrap().next_warm, Some(1));
            assert!(core.poll_host_analysis().is_none());
        }
        lease.generation = 2;
        let intermediate = MediaId::parse("fixture:track:intermediate").unwrap();
        clock.sequence = 2;
        clock.queue_epoch = Some(8);
        clock.media_id = Some(intermediate.clone());
        clock.control_uptime_ms = Some(1250);
        relative_frame(&mut core, &calls, clock.clone());
        lease.media_id = intermediate.clone();
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(
            core.host_analysis.lease.is_none(),
            "An intermediate URI before latest dispatch cannot acquire a lease"
        );
        calls.hold_relative.store(false, Ordering::Relaxed);
        calls
            .dispatch_relative(42, RemoteOutputControl::Next)
            .unwrap();
        core.poll_remote();
        relative_ack(&mut core, 1, true);
        relative_ack(&mut core, 2, true);
        clock.sequence = 3;
        clock.queue_epoch = Some(9);
        clock.media_id = Some(MediaId::parse("fixture:track:third").unwrap());
        clock.control_uptime_ms = Some(1500);
        relative_frame(&mut core, &calls, clock.clone());
        calls
            .dispatch_relative(43, RemoteOutputControl::Previous)
            .unwrap();
        core.poll_remote();
        clock.sequence = 4;
        clock.queue_epoch = Some(9);
        clock.media_id = Some(intermediate.clone());
        clock.control_uptime_ms = Some(1750);
        relative_frame(&mut core, &calls, clock.clone());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(
            core.host_analysis.lease.is_none(),
            "Playing alone cannot replace the latest dispatch receipt"
        );
        relative_ack(&mut core, 3, true);
        clock.queue_epoch = Some(9);
        clock.sequence = 5;
        relative_frame(&mut core, &calls, clock.clone());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(
            core.host_analysis.lease.is_none(),
            "A repeated URI needs a newer occurrence than the latest dispatch"
        );
        clock.queue_epoch = Some(10);
        clock.sequence = 6;
        relative_frame(&mut core, &calls, clock);
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert_eq!(slot.lock().unwrap().lease, Some(lease));
        assert_eq!(slot.lock().unwrap().next_warm, Some(1));
        assert!(core.host_analysis.warming_next.is_none());
    }

    #[test]
    fn manual_relative_contextless_loading_markers_keep_only_inert_original_standby() {
        let (mut core, slot, calls, mut lease, mut clock) = manual_relative_fixture();
        core.send_request(CommandRequest {
            request_id: 41,
            command: PlayerCommand::Next,
        });
        calls.hold_relative.store(true, Ordering::Relaxed);
        core.send_request(CommandRequest {
            request_id: 42,
            command: PlayerCommand::Next,
        });
        core.send_request(CommandRequest {
            request_id: 43,
            command: PlayerCommand::SkipPrevious,
        });
        let deadline = core.host_analysis.warming_next.as_ref().unwrap().deadline;
        let full_actions = clock.actions.clone();
        for (step, name) in [(1, "second"), (2, "third"), (3, "second")] {
            clock.sequence += 1;
            clock.queue_epoch = Some(7 + step);
            clock.control_uptime_ms = Some(1000 + 250 * step);
            clock.media_id = Some(MediaId::parse(format!("fixture:track:{name}")).unwrap());
            clock.state = PlaybackState::Loading;
            clock.rate = 0.0;
            clock.actions = None;
            clock.metadata.as_mut().unwrap().context_uri = None;
            relative_frame(&mut core, &calls, clock.clone());
            core.poll_analysis();
            assert_eq!(
                core.host_analysis.warming_next.as_ref().unwrap().deadline,
                deadline
            );
            assert!(slot.lock().unwrap().lease.is_none() && core.host_analysis.lease.is_none());
            assert_eq!(slot.lock().unwrap().next_warm, Some(1));
            assert!(core.poll_host_analysis().is_none());
            if step == 3 {
                // Even with the latest ACK, the marker itself is not Playing
                // and cannot acquire capture or replace its absent context.
                lease.generation = 2;
                lease.media_id = clock.media_id.clone().unwrap();
                core.update_host_analysis(Some(lease.clone())).unwrap();
                assert!(core.host_analysis.lease.is_none());
            }
            clock.sequence += 1;
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
            clock.actions = full_actions.clone();
            clock.metadata.as_mut().unwrap().context_uri = Some("fixture:album:relative".into());
            relative_frame(&mut core, &calls, clock.clone());
            if step < 3 {
                calls.hold_relative.store(false, Ordering::Relaxed);
                calls
                    .dispatch_relative(
                        41 + step,
                        if step == 1 {
                            RemoteOutputControl::Next
                        } else {
                            RemoteOutputControl::Previous
                        },
                    )
                    .unwrap();
                core.poll_remote();
                relative_ack(&mut core, step + 1, true);
            }
        }
        lease.generation = 3;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert_eq!(slot.lock().unwrap().lease, Some(lease));
        assert_eq!(slot.lock().unwrap().next_warm, Some(1));
    }

    #[test]
    fn manual_relative_loading_rejects_context_conflicts_and_missing_or_regressing_scope() {
        for fault in [
            "full contextless",
            "wrong context",
            "missing metadata",
            "missing uptime",
            "missing epoch",
            "epoch rollback",
            "uptime rollback",
            "sequence rollback",
            "runtime",
            "device",
            "unidentified",
        ] {
            let (mut core, slot, calls, _, mut clock) = manual_relative_fixture();
            core.send_request(CommandRequest {
                request_id: 41,
                command: PlayerCommand::Next,
            });
            core.send_request(CommandRequest {
                request_id: 42,
                command: PlayerCommand::Next,
            });
            clock.sequence = 2;
            clock.queue_epoch = Some(8);
            clock.control_uptime_ms = Some(1250);
            clock.media_id = Some(MediaId::parse("fixture:track:second").unwrap());
            clock.state = PlaybackState::Loading;
            clock.rate = 0.0;
            clock.actions = None;
            clock.metadata.as_mut().unwrap().context_uri = None;
            relative_frame(&mut core, &calls, clock.clone());
            assert!(core.host_analysis.warming_next.is_some());
            clock.sequence = 3;
            clock.queue_epoch = Some(9);
            clock.control_uptime_ms = Some(1500);
            clock.media_id = Some(MediaId::parse("fixture:track:third").unwrap());
            match fault {
                "full contextless" => clock.actions = Some(Default::default()),
                "wrong context" => {
                    clock.metadata.as_mut().unwrap().context_uri =
                        Some("fixture:album:other".into())
                }
                "missing metadata" => clock.metadata = None,
                "missing uptime" => clock.control_uptime_ms = None,
                "missing epoch" => clock.queue_epoch = None,
                "epoch rollback" => clock.queue_epoch = Some(7),
                "uptime rollback" => clock.control_uptime_ms = Some(1000),
                "sequence rollback" => clock.sequence = 1,
                "runtime" => clock.generation += 1,
                "device" => clock.device_id = "foreign".into(),
                "unidentified" => clock.media_id = None,
                _ => unreachable!(),
            }
            observe_spatial_next_clock(&mut core, clock);
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert!(slot.lock().unwrap().next_warm.is_none(), "{fault}");
        }
    }

    #[test]
    fn manual_relative_dispatch_observation_accepts_full_loading_and_newer_same_occurrence() {
        let (mut core, slot, calls, mut lease, mut clock) = manual_relative_fixture();
        core.send_request(CommandRequest {
            request_id: 41,
            command: PlayerCommand::Next,
        });
        calls.hold_relative.store(true, Ordering::Relaxed);
        core.send_request(CommandRequest {
            request_id: 42,
            command: PlayerCommand::Next,
        });
        core.send_request(CommandRequest {
            request_id: 43,
            command: PlayerCommand::SkipPrevious,
        });
        let deadline = core.host_analysis.warming_next.as_ref().unwrap().deadline;
        clock.sequence = 2;
        clock.queue_epoch = Some(8);
        clock.control_uptime_ms = Some(1250);
        clock.media_id = Some(MediaId::parse("fixture:track:second").unwrap());
        clock.state = PlaybackState::Loading;
        clock.rate = 0.0;
        relative_frame(&mut core, &calls, clock.clone());
        calls.hold_relative.store(false, Ordering::Relaxed);
        calls
            .dispatch_relative(42, RemoteOutputControl::Next)
            .unwrap();
        core.poll_remote();
        let manual = core
            .host_analysis
            .warming_next
            .as_ref()
            .unwrap()
            .manual
            .as_ref()
            .unwrap();
        assert_eq!(manual.requests[1].dispatch.as_ref().unwrap().sequence, 2);
        relative_ack(&mut core, 2, true);
        assert!(core.host_analysis.lease.is_none() && slot.lock().unwrap().lease.is_none());
        clock.sequence = 3;
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        relative_frame(&mut core, &calls, clock.clone());
        clock.sequence = 4;
        clock.queue_epoch = Some(9);
        clock.control_uptime_ms = Some(1500);
        clock.media_id = Some(MediaId::parse("fixture:track:third").unwrap());
        relative_frame(&mut core, &calls, clock.clone());
        calls
            .dispatch_relative(43, RemoteOutputControl::Previous)
            .unwrap();
        // The provider pinned dispatch sequence4/epoch9, but its event can be
        // observed after one or two newer full snapshots of that same item.
        for sequence in [5, 6] {
            clock.sequence = sequence;
            relative_frame(&mut core, &calls, clock.clone());
        }
        core.poll_remote();
        relative_ack(&mut core, 3, true);
        let warming = core.host_analysis.warming_next.as_ref().unwrap();
        assert_eq!(warming.deadline, deadline);
        let dispatch = warming.manual.as_ref().unwrap().requests[2]
            .dispatch
            .as_ref()
            .unwrap();
        assert_eq!((dispatch.sequence, dispatch.epoch), (4, 9));
        lease.generation = 2;
        lease.media_id = clock.media_id.clone().unwrap();
        core.update_host_analysis(Some(lease.clone())).unwrap();
        assert!(
            core.host_analysis.lease.is_none(),
            "Newer observations in the dispatch occurrence cannot grant a successor lease"
        );
        clock.sequence = 7;
        clock.queue_epoch = Some(10);
        clock.control_uptime_ms = Some(1750);
        clock.media_id = Some(MediaId::parse("fixture:track:second").unwrap());
        relative_frame(&mut core, &calls, clock.clone());
        lease.media_id = clock.media_id.unwrap();
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();
        assert_eq!(slot.lock().unwrap().lease, Some(lease));
        assert_eq!(slot.lock().unwrap().next_warm, Some(1));
    }

    fn dispatched_relative_successor_loading(
        full: bool,
    ) -> (
        PlaybackCoordinator,
        Arc<Mutex<Slot>>,
        Arc<ResumeCalls>,
        HostAnalysisLease,
        RemoteOutputClock,
    ) {
        let (mut core, slot, calls, lease, mut clock) = manual_relative_fixture();
        core.send_request(CommandRequest {
            request_id: 41,
            command: PlayerCommand::Next,
        });
        calls.hold_relative.store(true, Ordering::Relaxed);
        core.send_request(CommandRequest {
            request_id: 42,
            command: PlayerCommand::Next,
        });
        clock.sequence = 2;
        clock.queue_epoch = Some(8);
        clock.control_uptime_ms = Some(1250);
        clock.media_id = Some(MediaId::parse("fixture:track:second").unwrap());
        relative_frame(&mut core, &calls, clock.clone());
        calls.hold_relative.store(false, Ordering::Relaxed);
        calls
            .dispatch_relative(42, RemoteOutputControl::Next)
            .unwrap();
        core.poll_remote();
        relative_ack(&mut core, 2, true);
        clock.sequence = 3;
        clock.queue_epoch = Some(9);
        clock.control_uptime_ms = Some(1500);
        clock.media_id = Some(MediaId::parse("fixture:track:third").unwrap());
        clock.state = PlaybackState::Loading;
        clock.rate = 0.0;
        if !full {
            clock.actions = None;
            clock.metadata.as_mut().unwrap().context_uri = None;
        }
        relative_frame(&mut core, &calls, clock.clone());
        calls.hold_relative.store(true, Ordering::Relaxed);
        (core, slot, calls, lease, clock)
    }

    #[test]
    fn manual_relative_loading_input_retains_only_previously_dispatched_successor_standby() {
        for full in [false, true] {
            let (mut core, slot, calls, mut lease, mut clock) =
                dispatched_relative_successor_loading(full);
            let deadline = core.host_analysis.warming_next.as_ref().unwrap().deadline;
            core.send_request(CommandRequest {
                request_id: 43,
                command: PlayerCommand::SkipPrevious,
            });
            assert_eq!(
                core.host_analysis.warming_next.as_ref().unwrap().deadline,
                deadline
            );
            assert_eq!(
                core.host_analysis
                    .warming_next
                    .as_ref()
                    .unwrap()
                    .manual
                    .as_ref()
                    .unwrap()
                    .requests
                    .len(),
                3
            );
            assert_eq!(
                calls.relative_requests.lock().unwrap().last(),
                Some(&(43, RemoteOutputControl::Previous))
            );
            assert!(core.host_analysis.lease.is_none() && slot.lock().unwrap().lease.is_none());
            assert!(core.poll_host_analysis().is_none());
            // No dispatch/lease authority comes from the inert Loading input.
            assert!(
                core.host_analysis
                    .warming_next
                    .as_ref()
                    .unwrap()
                    .manual
                    .as_ref()
                    .unwrap()
                    .requests[2]
                    .dispatch
                    .is_none()
            );
            clock.sequence = 4;
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
            clock.actions = Some(RemoteOutputActions {
                previous: true,
                next: true,
                ..Default::default()
            });
            clock.metadata.as_mut().unwrap().context_uri = Some("fixture:album:relative".into());
            relative_frame(&mut core, &calls, clock.clone());
            calls.hold_relative.store(false, Ordering::Relaxed);
            calls
                .dispatch_relative(43, RemoteOutputControl::Previous)
                .unwrap();
            core.poll_remote();
            relative_ack(&mut core, 3, true);
            lease.generation = 2;
            lease.media_id = clock.media_id.clone().unwrap();
            core.update_host_analysis(Some(lease.clone())).unwrap();
            assert!(
                core.host_analysis.lease.is_none(),
                "The dispatched occurrence itself still cannot claim capture"
            );
            clock.sequence = 5;
            clock.queue_epoch = Some(10);
            clock.control_uptime_ms = Some(1750);
            clock.media_id = Some(MediaId::parse("fixture:track:second").unwrap());
            relative_frame(&mut core, &calls, clock.clone());
            lease.media_id = clock.media_id.unwrap();
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_analysis();
            assert_eq!(slot.lock().unwrap().lease, Some(lease));
            assert_eq!(slot.lock().unwrap().next_warm, Some(1));
        }
    }

    #[test]
    fn manual_relative_loading_input_rejects_queued_only_unobserved_or_unadvanced_scope() {
        for fault in [
            "latest not dispatched",
            "unadvanced epoch",
            "unadvanced sequence",
            "unobserved media",
            "unobserved uptime",
            "stale",
            "context conflict",
            "auth",
            "control",
            "runtime",
            "device",
            "deadline",
            "same origin",
        ] {
            let (mut core, slot, _, _, _) = dispatched_relative_successor_loading(false);
            let manual = core
                .host_analysis
                .warming_next
                .as_mut()
                .unwrap()
                .manual
                .as_mut()
                .unwrap();
            match fault {
                "latest not dispatched" => manual.requests.last_mut().unwrap().dispatch = None,
                "unadvanced epoch" => {
                    manual.last_epoch = 8;
                    core.host_analysis
                        .output_clock
                        .as_mut()
                        .unwrap()
                        .queue_epoch = Some(8);
                }
                "unadvanced sequence" => {
                    manual.last_sequence = 2;
                    core.host_analysis.output_clock.as_mut().unwrap().sequence = 2;
                }
                "unobserved media" => {
                    manual.last_media = Some(MediaId::parse("fixture:track:other").unwrap())
                }
                "unobserved uptime" => manual.last_uptime_ms -= 1,
                "stale" => {
                    core.host_analysis.output_clock.as_mut().unwrap().observed -=
                        LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "context conflict" => {
                    core.remote
                        .states
                        .get_mut("fixture")
                        .unwrap()
                        .playback
                        .as_mut()
                        .unwrap()
                        .context_uri = Some("fixture:album:foreign".into())
                }
                "auth" => {
                    core.remote
                        .states
                        .get_mut("fixture")
                        .unwrap()
                        .authentication_required = true
                }
                "control" => {
                    core.remote
                        .states
                        .get_mut("fixture")
                        .unwrap()
                        .owned_output_control = false
                }
                "runtime" => core.host_analysis.output_clock.as_mut().unwrap().generation += 1,
                "device" => {
                    core.host_analysis.output_clock.as_mut().unwrap().device = "foreign".into()
                }
                "deadline" => {
                    core.host_analysis.warming_next.as_mut().unwrap().deadline = Instant::now()
                }
                "same origin" => {
                    manual.last_media.clone_from(&manual.origin.media);
                    core.host_analysis
                        .output_clock
                        .as_mut()
                        .unwrap()
                        .media
                        .clone_from(&manual.origin.media);
                }
                _ => unreachable!(),
            }
            core.send_request(CommandRequest {
                request_id: 43,
                command: PlayerCommand::SkipPrevious,
            });
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert!(slot.lock().unwrap().next_warm.is_none(), "{fault}");
        }
    }

    #[test]
    fn manual_relative_dispatch_observation_rejects_missing_or_conflicting_pinned_occurrence() {
        for fault in [
            "missing epoch",
            "zero epoch",
            "rollback epoch",
            "required conflict",
            "media conflict",
            "device conflict",
            "runtime conflict",
            "future sequence",
            "old sequence",
            "stale observation",
            "missing full",
            "context conflict",
        ] {
            let (mut core, slot, calls, _, mut clock) = manual_relative_fixture();
            core.send_request(CommandRequest {
                request_id: 41,
                command: PlayerCommand::Next,
            });
            calls.hold_relative.store(true, Ordering::Relaxed);
            core.send_request(CommandRequest {
                request_id: 42,
                command: PlayerCommand::Next,
            });
            clock.sequence = 2;
            clock.queue_epoch = Some(8);
            clock.control_uptime_ms = Some(1250);
            clock.media_id = Some(MediaId::parse("fixture:track:second").unwrap());
            clock.state = PlaybackState::Loading;
            clock.rate = 0.0;
            relative_frame(&mut core, &calls, clock);
            calls.hold_relative.store(false, Ordering::Relaxed);
            calls
                .dispatch_relative(42, RemoteOutputControl::Next)
                .unwrap();
            {
                let mut updates = calls.relative_updates.lock().unwrap();
                let RemoteUpdate::OutputCommand(command) = updates.front_mut().unwrap() else {
                    unreachable!()
                };
                match fault {
                    "missing epoch" => command.observed_queue_epoch = None,
                    "zero epoch" => command.observed_queue_epoch = Some(0),
                    "rollback epoch" => command.observed_queue_epoch = Some(7),
                    "required conflict" => command.required_queue_epoch = Some(9),
                    "media conflict" => {
                        command.media_id = MediaId::parse("fixture:track:other").unwrap()
                    }
                    "device conflict" => command.device_id = "foreign".into(),
                    "runtime conflict" => command.generation += 1,
                    "future sequence" => command.sequence += 1,
                    "old sequence" => command.sequence = 0,
                    "stale observation" => {
                        core.host_analysis.output_clock.as_mut().unwrap().observed -=
                            LEASE_FRESHNESS + Duration::from_millis(1)
                    }
                    "missing full" => {
                        core.host_analysis.output_clock.as_mut().unwrap().full = false
                    }
                    "context conflict" => {
                        core.host_analysis.output_clock.as_mut().unwrap().context =
                            Some("fixture:album:other".into())
                    }
                    _ => unreachable!(),
                }
            }
            core.poll_remote();
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert!(slot.lock().unwrap().next_warm.is_none(), "{fault}");
            assert!(slot.lock().unwrap().lease.is_none(), "{fault}");
        }
    }

    #[test]
    fn manual_relative_continuation_rejects_unproven_or_changed_scope_without_renewing_deadline() {
        for fault in [
            "no dispatch",
            "no clock",
            "stale",
            "runtime",
            "device",
            "paused",
            "auth",
            "control",
            "pending start",
            "restore",
            "natural",
            "deadline",
            "sync reject",
            "limit",
        ] {
            let (mut core, slot, calls, _, _) = manual_relative_fixture();
            if fault == "no dispatch" {
                calls.hold_relative.store(true, Ordering::Relaxed);
            }
            core.send_request(CommandRequest {
                request_id: 41,
                command: PlayerCommand::Next,
            });
            match fault {
                "no clock" => core.host_analysis.output_clock = None,
                "stale" => {
                    core.host_analysis.output_clock.as_mut().unwrap().observed -=
                        LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "runtime" => core.host_analysis.output_clock.as_mut().unwrap().generation += 1,
                "device" => {
                    core.host_analysis.output_clock.as_mut().unwrap().device = "foreign".into()
                }
                "paused" => {
                    core.host_analysis
                        .output_clock
                        .as_mut()
                        .unwrap()
                        .moving_playback = false
                }
                "auth" => {
                    core.remote
                        .states
                        .get_mut("fixture")
                        .unwrap()
                        .authentication_required = true
                }
                "control" => {
                    core.remote
                        .states
                        .get_mut("fixture")
                        .unwrap()
                        .owned_output_control = false
                }
                "pending start" => calls.pending_start.store(true, Ordering::Relaxed),
                "restore" => {
                    core.remote.candidate = Some(RemoteSelection {
                        provider: "fixture".into(),
                        current: core.snapshot().queue.current.unwrap(),
                        position_ms: 0,
                        context_uri: None,
                        account_id: "original-fixture-listener".into(),
                        stable_account_id: None,
                    })
                }
                "natural" => {
                    let origin = core.host_analysis.output_clock.as_ref().unwrap().clone();
                    core.host_analysis.warming_next.as_mut().unwrap().natural =
                        Some(NaturalSpatialNext {
                            origin,
                            context: "fixture:album:relative".into(),
                            successor: None,
                        });
                }
                "deadline" => {
                    core.host_analysis.warming_next.as_mut().unwrap().deadline = Instant::now()
                }
                "sync reject" => calls.reject_relative.store(true, Ordering::Relaxed),
                "limit" => {
                    for request_id in 42..49 {
                        core.send_request(CommandRequest {
                            request_id,
                            command: PlayerCommand::Next,
                        });
                    }
                }
                _ => {}
            }
            core.send_request(CommandRequest {
                request_id: 49,
                command: PlayerCommand::Next,
            });
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert!(slot.lock().unwrap().next_warm.is_none(), "{fault}");
        }
    }

    #[test]
    fn manual_relative_chain_cancels_on_exact_rejection_and_conservative_competing_intents() {
        for fault in [
            "latest reject",
            "earlier reject",
            "async previous",
            "pause",
            "seek",
            "previous restart",
            "stale after admission",
            "same origin",
        ] {
            let (mut core, slot, calls, mut lease, mut clock) = manual_relative_fixture();
            core.send_request(CommandRequest {
                request_id: 41,
                command: PlayerCommand::Next,
            });
            core.send_request(CommandRequest {
                request_id: 42,
                command: PlayerCommand::SkipPrevious,
            });
            assert!(core.host_analysis.warming_next.is_some());
            match fault {
                "latest reject" => relative_ack(&mut core, 2, false),
                "earlier reject" => relative_ack(&mut core, 1, false),
                "async previous" => {
                    calls.relative_updates.lock().unwrap().push_back(
                        RemoteUpdate::OutputCommandFailed {
                            control: RemoteOutputControl::Previous,
                            error: PlayerError::invalid("Original async relative rejection"),
                        },
                    );
                    core.poll_remote();
                }
                "pause" => {
                    core.send(PlayerCommand::Pause).unwrap();
                }
                "seek" => {
                    core.send(PlayerCommand::Seek { position_ms: 0 }).unwrap();
                }
                "previous restart" => {
                    core.send(PlayerCommand::Previous).unwrap();
                }
                "stale after admission" => {
                    core.host_analysis.output_clock.as_mut().unwrap().observed -=
                        LEASE_FRESHNESS + Duration::from_millis(1);
                    core.revoke_stale_host_analysis();
                }
                "same origin" => {
                    relative_ack(&mut core, 2, true);
                    clock.sequence = 2;
                    clock.queue_epoch = Some(8);
                    clock.control_uptime_ms = Some(1250);
                    relative_frame(&mut core, &calls, clock);
                    lease.generation = 2;
                    core.update_host_analysis(Some(lease)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert!(slot.lock().unwrap().next_warm.is_none(), "{fault}");
        }
    }

    fn setup_paused_resume() -> (PlaybackCoordinator, Arc<Mutex<Slot>>, Arc<ResumeCalls>) {
        let (mut core, slot, mut lease) = setup();
        let calls = Arc::new(ResumeCalls::default());
        core.register_remote(ResumeProvider(calls.clone()));
        lease.spatial_stereo = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        let state = core.remote.states.get_mut(&lease.provider).unwrap();
        state.owned_output_control = true;
        let playback = state.playback.as_mut().unwrap();
        playback.state = PlaybackState::Paused;
        playback.position_rate = 0.0;
        lease.playing = false;
        core.update_host_analysis(Some(lease)).unwrap();
        core.poll_analysis();
        slot.lock().unwrap().resume = SpatialResume::Warming;
        (core, slot, calls)
    }

    fn paused_output_clock(lease: &HostAnalysisLease) -> RemoteOutputClock {
        RemoteOutputClock {
            generation: 2,
            sequence: 1,
            device_id: lease.device_id.clone(),
            media_id: Some(lease.media_id.clone()),
            state: PlaybackState::Paused,
            position_ms: 1500,
            rate: 0.0,
            active: true,
            volume: None,
            options: None,
            actions: None,
            metadata: None,
            control_uptime_ms: Some(1000),
            queue_epoch: Some(3),
            confirmed_same_media_next: None,
        }
    }

    fn capture_witness_origin() -> (
        PlaybackCoordinator,
        Arc<Mutex<Slot>>,
        HostAnalysisLease,
        RemoteOutputClock,
        Arc<ResumeCalls>,
    ) {
        let (mut core, slot, mut lease) = setup();
        lease.spatial_stereo = true;
        let calls = Arc::new(ResumeCalls::default());
        core.register_remote(ResumeProvider(calls.clone()));
        let state = core.remote.states.get_mut(&lease.provider).unwrap();
        state.owned_output_control = false;
        state.playback.as_mut().unwrap().context_uri = Some(NATURAL_TEST_CONTEXT.into());
        let mut clock = paused_output_clock(&lease);
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        clock.actions = Some(RemoteOutputActions {
            pause: true,
            ..Default::default()
        });
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Owned restart".into(),
            artists: vec![],
            album: None,
            artwork: None,
            duration_ms: 80_470,
            context_uri: Some(NATURAL_TEST_CONTEXT.into()),
        });
        *calls.capture_witness.lock().unwrap() = Some(OwnedCaptureWitness {
            id: 7,
            generation: clock.generation,
            device_id: clock.device_id.clone(),
            media_id: lease.media_id.clone(),
            queue_epoch: clock.queue_epoch.unwrap(),
            context_uri: NATURAL_TEST_CONTEXT.into(),
            playing_sequence: clock.sequence,
            playing_control_uptime_ms: clock.control_uptime_ms.unwrap(),
        });
        (core, slot, lease, clock, calls)
    }

    #[test]
    fn capture_witness_seeds_playing_before_an_immediate_pause_without_granting_controls() {
        let (mut core, slot, mut lease, mut clock, calls) = capture_witness_origin();
        observe_spatial_next_clock(&mut core, clock.clone());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        // No analysis poll between the actual Playing lease and Pause.
        assert!(core.host_analysis.renderer_only_pause.is_some());
        let frame = core
            .remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        frame.state = PlaybackState::Paused;
        frame.position_rate = 0.0;
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        clock.sequence += 1;
        clock.control_uptime_ms = Some(1_050);
        observe_spatial_next_clock(&mut core, clock.clone());
        lease.playing = false;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();
        assert_eq!(
            slot.lock().unwrap().confirmed_renderer_only_pause,
            Some(lease.generation)
        );
        assert!(!core.remote.states[&lease.provider].owned_output_control);
        assert!(!core.prepare_host_spatial_next(None));
        assert!(calls.actions.lock().unwrap().is_empty());

        // A previously confirmed pause can outlive the short selection witness;
        // the immutable capture proof remains scoped while fresh raw clocks arrive.
        core.host_analysis
            .renderer_only_pause
            .as_mut()
            .unwrap()
            .playing_observed -= Duration::from_secs(46);
        clock.sequence += 1;
        clock.actions = None;
        clock.metadata = None;
        clock.control_uptime_ms = Some(47_000);
        observe_spatial_next_clock(&mut core, clock);
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();
        assert_eq!(
            slot.lock().unwrap().confirmed_renderer_only_pause,
            Some(lease.generation)
        );
        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .owned_output_control = true;
        core.poll_host_analysis();
        assert_eq!(
            slot.lock().unwrap().confirmed_renderer_only_pause,
            Some(lease.generation),
            "a later cloud control binding preserves the same capture witness"
        );
        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .owned_output_control = false;
        *calls.capture_witness.lock().unwrap() = None;
        core.poll_host_analysis();
        assert_eq!(slot.lock().unwrap().confirmed_renderer_only_pause, None);
    }

    #[test]
    fn capture_witness_cannot_seed_a_paused_restore_or_a_mismatched_playing_occurrence() {
        for missing in [
            "paused restore",
            "id",
            "runtime",
            "device",
            "media",
            "epoch",
            "sequence",
            "uptime",
            "context",
            "auth",
            "stale clock",
        ] {
            let (mut core, slot, mut lease, mut clock, calls) = capture_witness_origin();
            match missing {
                "paused restore" => {
                    lease.playing = false;
                    clock.state = PlaybackState::Paused;
                    clock.rate = 0.0;
                    let frame = core
                        .remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .playback
                        .as_mut()
                        .unwrap();
                    frame.state = PlaybackState::Paused;
                    frame.position_rate = 0.0;
                }
                "auth" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .authentication_required = true
                }
                "stale clock" => {}
                kind => {
                    let mut proof = calls.capture_witness.lock().unwrap();
                    let proof = proof.as_mut().unwrap();
                    match kind {
                        "id" => proof.id = 0,
                        "runtime" => proof.generation += 1,
                        "device" => proof.device_id = "foreign".into(),
                        "media" => {
                            proof.media_id = MediaId::parse("fixture:track:foreign").unwrap()
                        }
                        "epoch" => proof.queue_epoch += 1,
                        "sequence" => proof.playing_sequence += 1,
                        "uptime" => proof.playing_control_uptime_ms += 1,
                        "context" => proof.context_uri = "fixture:album:foreign".into(),
                        _ => unreachable!(),
                    }
                }
            }
            observe_spatial_next_clock(&mut core, clock);
            if missing == "stale clock" {
                core.host_analysis.output_clock.as_mut().unwrap().observed -=
                    LEASE_FRESHNESS + Duration::from_millis(1);
            }
            core.update_host_analysis(Some(lease)).unwrap();
            core.poll_host_analysis();
            assert!(
                core.host_analysis.renderer_only_pause.is_none(),
                "{missing}"
            );
            assert_eq!(
                slot.lock().unwrap().confirmed_renderer_only_pause,
                None,
                "{missing}"
            );
        }
    }

    #[test]
    fn a_reissued_capture_witness_cannot_hold_the_previous_paused_graph() {
        for partial in [true, false] {
            let (mut core, _slot, mut lease, mut clock, calls) = capture_witness_origin();
            observe_spatial_next_clock(&mut core, clock.clone());
            core.update_host_analysis(Some(lease.clone())).unwrap();
            let frame = core
                .remote
                .states
                .get_mut(&lease.provider)
                .unwrap()
                .playback
                .as_mut()
                .unwrap();
            frame.state = PlaybackState::Paused;
            frame.position_rate = 0.0;
            clock.sequence += 1;
            clock.state = PlaybackState::Paused;
            clock.rate = 0.0;
            observe_spatial_next_clock(&mut core, clock.clone());
            lease.playing = false;
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_host_analysis();
            assert!(
                core.host_analysis
                    .renderer_only_pause
                    .as_ref()
                    .unwrap()
                    .confirmed
            );
            // Same raw output scope cannot renew the authority identity.
            calls.capture_witness.lock().unwrap().as_mut().unwrap().id += 1;
            core.arm_spatial_resume_clock_gap();
            assert!(core.host_analysis.resume_clock_gap.is_none());
            clock.sequence += 1;
            clock.state = PlaybackState::Playing;
            clock.rate = if partial { 0.0 } else { 1.0 };
            if partial {
                clock.actions = None;
            }
            observe_spatial_next_clock(&mut core, clock);
            assert!(!core.waiting_for_playing_host_lease(&lease));
            assert!(!core.renderer_only_pause_proof(&lease, false));
            assert!(core.host_analysis.renderer_only_pause.is_none());
        }
    }

    fn observe_spatial_next_clock(core: &mut PlaybackCoordinator, clock: RemoteOutputClock) {
        core.check_host_spatial_clock(&PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::OutputClock { clock: Some(clock) },
        });
    }

    const NATURAL_TEST_CONTEXT: &str = "spotify:album:5ht7ItJgpBH7W6vJ5BqpPr";
    const NATURAL_TEST_DURATION: u64 = 80_470;

    fn natural_next_origin() -> (
        PlaybackCoordinator,
        Arc<Mutex<Slot>>,
        HostAnalysisLease,
        RemoteOutputClock,
    ) {
        let (mut core, slot, mut lease) = setup();
        core.register_remote(ResumeProvider(Arc::new(ResumeCalls::default())));
        let state = core.remote.states.get_mut(&lease.provider).unwrap();
        state.owned_output_control = true;
        let playback = state.playback.as_mut().unwrap();
        playback.context_uri = Some(NATURAL_TEST_CONTEXT.into());
        playback.position_ms = NATURAL_TEST_DURATION - 150;
        playback.current.as_mut().unwrap().duration_ms = NATURAL_TEST_DURATION;
        playback.transport.next = true;
        lease.spatial_stereo = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();

        let mut clock = paused_output_clock(&lease);
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        clock.position_ms = NATURAL_TEST_DURATION - 150;
        clock.actions = Some(RemoteOutputActions {
            next: true,
            pause: true,
            ..Default::default()
        });
        clock.metadata = Some(RemoteOutputMetadata {
            title: "Original owned fixture".into(),
            artists: vec!["Fixture Artist".into()],
            album: None,
            duration_ms: NATURAL_TEST_DURATION,
            artwork: None,
            context_uri: Some(NATURAL_TEST_CONTEXT.into()),
        });
        (core, slot, lease, clock)
    }

    fn natural_next_loading(mut playing: RemoteOutputClock) -> RemoteOutputClock {
        playing.sequence += 1;
        playing.control_uptime_ms = Some(1_150);
        playing.state = PlaybackState::Loading;
        playing.rate = 0.0;
        playing.position_ms = NATURAL_TEST_DURATION - 1;
        playing
    }

    #[test]
    fn spatial_clock_change_only_reuses_the_exact_fresh_applied_occurrence() {
        for condition in [
            "exact",
            "web backoff",
            "unapplied",
            "stale origin",
            "other occurrence",
            "other runtime",
            "other context",
            "rate",
            "no drift",
            "newer clock",
            "pause",
        ] {
            let (mut core, slot, mut lease, mut clock) = natural_next_origin();
            let output_clock = |clock: &RemoteOutputClock| PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                },
            };
            core.send(output_clock(&clock)).unwrap();
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_host_analysis();
            let origin_sequence = clock.sequence;
            let retired_generation = lease.generation;
            clock.sequence += 1;
            clock.control_uptime_ms = Some(1_017);
            clock.position_ms -= 919;
            let sequence = clock.sequence;
            match condition {
                "web backoff" => {
                    core.remote.states.get_mut("fixture").unwrap().connection =
                        ProviderConnectionState::Unavailable
                }
                "stale origin" => {
                    core.host_analysis
                        .leased_output_clock
                        .as_mut()
                        .unwrap()
                        .observed -= LEASE_FRESHNESS + Duration::from_millis(1);
                }
                "other occurrence" => clock.queue_epoch = Some(4),
                "other runtime" => clock.generation += 1,
                "other context" => {
                    clock.metadata.as_mut().unwrap().context_uri =
                        Some("fixture:album:other".into())
                }
                "rate" => clock.rate = 0.5,
                "no drift" => clock.position_ms += 910,
                "pause" => {
                    core.send(PlayerCommand::Pause).unwrap();
                    assert!(core.host_analysis.pause_pending_until.is_some());
                }
                _ => {}
            }
            if condition == "unapplied" {
                core.check_host_spatial_clock(&output_clock(&clock));
            } else {
                core.send(output_clock(&clock)).unwrap();
            }
            if condition == "newer clock" {
                clock.sequence += 1;
                core.send(output_clock(&clock)).unwrap();
            }
            lease.generation += 1;
            core.send(PlayerCommand::HostAnalysisAfterClockChange {
                lease: lease.clone(),
                retired_generation,
                origin_sequence,
                sequence,
            })
            .unwrap();
            core.poll_host_analysis();
            let slot = slot.lock().unwrap();
            assert_eq!(
                slot.reanchor,
                matches!(condition, "exact" | "web backoff")
                    .then_some((retired_generation, lease.generation)),
                "{condition}"
            );
            let expected = match condition {
                "pause" => Some(retired_generation),
                "unapplied" | "newer clock" => None,
                _ => Some(lease.generation),
            };
            assert_eq!(
                slot.lease.as_ref().map(|lease| lease.generation),
                expected,
                "{condition}"
            );
        }
    }

    fn natural_next_successor(mut loading: RemoteOutputClock) -> RemoteOutputClock {
        loading.sequence += 1;
        loading.control_uptime_ms = Some(1_600);
        loading.queue_epoch = loading.queue_epoch.map(|epoch| epoch + 1);
        loading.media_id = Some(MediaId::parse("fixture:track:natural-next").unwrap());
        loading.state = PlaybackState::Playing;
        loading.rate = 1.0;
        loading.position_ms = 1;
        loading
    }

    fn natural_successor_queue(clock: &RemoteOutputClock) -> RemoteOutputQueue {
        RemoteOutputQueue {
            generation: clock.generation,
            device_id: clock.device_id.clone(),
            media_id: clock.media_id.clone().unwrap(),
            epoch: clock.queue_epoch.unwrap(),
            sequence: 1,
            autoplay_ready: false,
            previous: vec![],
            upcoming: vec![player_domain::RemoteOutputQueueEntry {
                media_id: MediaId::parse("fixture:track:natural-next").unwrap(),
                source: Some(player_domain::QueueEntrySource::Context),
                metadata: None,
            }],
        }
    }

    #[test]
    fn successor_first_eof_projects_a_fresh_full_origin_without_refreshing_it() {
        let (mut core, slot, lease, mut clock) = natural_next_origin();
        clock.position_ms = NATURAL_TEST_DURATION - 1_100;
        observe_spatial_next_clock(&mut core, clock.clone());
        let observed = Instant::now() - Duration::from_millis(214);
        core.host_analysis.output_clock.as_mut().unwrap().observed = observed;
        core.observe_spatial_successor_queue(&lease.provider, &natural_successor_queue(&clock));

        let mut successor = natural_next_successor(clock.clone());
        successor.control_uptime_ms = clock.control_uptime_ms.map(|time| time + 214);
        successor.state = PlaybackState::Loading;
        successor.rate = 0.0;
        successor.position_ms = 0;
        successor.actions = None;
        successor.metadata = None;
        observe_spatial_next_clock(&mut core, successor);
        assert_eq!(
            slot.lock().unwrap().natural_next_warm,
            Some(lease.generation)
        );
        assert!(
            slot.lock().unwrap().lease.is_none(),
            "tail projection cannot acquire the successor capture lease"
        );
        let origin = &core
            .host_analysis
            .warming_next
            .as_ref()
            .unwrap()
            .natural
            .as_ref()
            .unwrap()
            .origin;
        assert_eq!(origin.position_ms, clock.position_ms);
        assert_eq!(origin.observed, observed);
        assert_eq!(origin.control_uptime_ms, clock.control_uptime_ms);
    }

    #[test]
    fn successor_first_tail_allows_only_bounded_outgoing_metadata_skew_for_the_exact_head() {
        for (duration, position, delta, expected) in [
            (125_504, 124_271, 215, true), // Live projection: 1,018 ms remain.
            (105_230, 104_082, 98, true),  // Live projection: 1,050 ms remain.
            (80_000, 78_750, 0, true),
            (80_000, 78_749, 0, false),
        ] {
            let (mut core, slot, lease, mut clock) = natural_next_origin();
            core.remote
                .states
                .get_mut(&lease.provider)
                .unwrap()
                .playback
                .as_mut()
                .unwrap()
                .current
                .as_mut()
                .unwrap()
                .duration_ms = duration;
            clock.metadata.as_mut().unwrap().duration_ms = duration;
            clock.position_ms = position;
            observe_spatial_next_clock(&mut core, clock.clone());
            let observed = Instant::now() - Duration::from_millis(delta.max(1));
            core.host_analysis.output_clock.as_mut().unwrap().observed = observed;
            core.observe_spatial_successor_queue(&lease.provider, &natural_successor_queue(&clock));
            let mut successor = natural_next_successor(clock.clone());
            successor.control_uptime_ms = clock.control_uptime_ms.map(|time| time + delta);
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            successor.actions = None;
            successor.metadata = None;
            observe_spatial_next_clock(&mut core, successor.clone());
            assert_eq!(
                slot.lock().unwrap().natural_next_warm.is_some(),
                expected,
                "duration={duration}, position={position}, delta={delta}"
            );
            if expected {
                assert!(slot.lock().unwrap().lease.is_none());
                let warming = core.host_analysis.warming_next.as_ref().unwrap();
                let deadline = warming.deadline;
                assert_eq!(warming.natural.as_ref().unwrap().origin.observed, observed);
                successor.sequence += 1;
                observe_spatial_next_clock(&mut core, successor);
                assert_eq!(
                    core.host_analysis.warming_next.as_ref().unwrap().deadline,
                    deadline
                );
            }
        }
        for fault in ["same URI", "incoming position", "wrong head"] {
            let (mut core, slot, lease, mut clock) = natural_next_origin();
            clock.position_ms = NATURAL_TEST_DURATION - 1050;
            observe_spatial_next_clock(&mut core, clock.clone());
            let mut queue = natural_successor_queue(&clock);
            if fault == "wrong head" {
                queue.upcoming[0].media_id = MediaId::parse("fixture:track:other").unwrap();
            }
            core.observe_spatial_successor_queue(&lease.provider, &queue);
            let mut successor = natural_next_successor(clock.clone());
            successor.control_uptime_ms = clock.control_uptime_ms;
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = if fault == "incoming position" {
                1001
            } else {
                0
            };
            successor.actions = None;
            successor.metadata = None;
            if fault == "same URI" {
                successor.media_id = clock.media_id;
                successor.queue_epoch = clock.queue_epoch;
                successor.position_ms = NATURAL_TEST_DURATION - 1050;
                successor.metadata = clock.metadata;
            }
            observe_spatial_next_clock(&mut core, successor);
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert!(slot.lock().unwrap().natural_next_warm.is_none(), "{fault}");
        }
    }

    #[test]
    fn successor_first_tail_projection_rejects_untrusted_or_overshooting_clocks() {
        for fault in [
            "backward uptime",
            "future uptime",
            "missing uptime",
            "stale origin",
            "zero rate",
            "negative rate",
            "nan rate",
            "infinite rate",
            "excessive rate",
            "overshoot",
        ] {
            let (mut core, slot, lease, mut clock) = natural_next_origin();
            clock.position_ms = NATURAL_TEST_DURATION - 1_100;
            match fault {
                "zero rate" => clock.rate = 0.0,
                "negative rate" => clock.rate = -1.0,
                "nan rate" => clock.rate = f64::NAN,
                "infinite rate" => clock.rate = f64::INFINITY,
                "excessive rate" => clock.rate = 4.01,
                "overshoot" => clock.position_ms = NATURAL_TEST_DURATION + 50,
                _ => {}
            }
            observe_spatial_next_clock(&mut core, clock.clone());
            core.host_analysis.output_clock.as_mut().unwrap().observed = Instant::now()
                - Duration::from_millis(if fault == "stale origin" { 801 } else { 214 });
            core.observe_spatial_successor_queue(&lease.provider, &natural_successor_queue(&clock));
            let mut successor = natural_next_successor(clock.clone());
            successor.control_uptime_ms = match fault {
                "backward uptime" => clock.control_uptime_ms.map(|time| time - 1),
                "future uptime" => clock.control_uptime_ms.map(|time| time + 801),
                "missing uptime" => None,
                _ => clock.control_uptime_ms.map(|time| time + 214),
            };
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            successor.actions = None;
            successor.metadata = None;
            observe_spatial_next_clock(&mut core, successor);
            assert!(core.host_analysis.warming_next.is_none(), "{fault}");
            assert_eq!(slot.lock().unwrap().natural_next_warm, None, "{fault}");
        }
    }

    #[test]
    fn tail_projection_does_not_count_forwarding_delay_twice_or_advance_from_age_alone() {
        let (_core, _slot, _lease, mut clock) = natural_next_origin();
        // The host already advanced a packet's 4,000 ms position by 114 ms
        // before it reached this coordinator. The next packet is 214 ms newer,
        // but only another 100 ms elapsed since the accepted old observation.
        clock.position_ms = 4_114;
        let mut origin = SpatialOutputClock::new("fixture", &clock);
        let now = Instant::now();
        origin.observed = now - Duration::from_millis(100);
        let observed = origin.observed;
        let mut next = natural_next_successor(clock.clone());
        next.control_uptime_ms = clock.control_uptime_ms.map(|time| time + 214);
        let projection = origin.projected_tail_position(&next, now).unwrap();
        assert_eq!(projection.control_delta_ms, 214);
        assert_eq!(projection.applied_delta_ms, 100);
        assert_eq!(projection.position_ms, 4_214);
        assert_eq!(origin.position_ms, 4_114);
        assert_eq!(origin.observed, observed);
        assert_eq!(origin.control_uptime_ms, clock.control_uptime_ms);

        next.control_uptime_ms = clock.control_uptime_ms;
        assert_eq!(
            origin
                .projected_tail_position(&next, now)
                .unwrap()
                .position_ms,
            4_114
        );
        next.control_uptime_ms = clock.control_uptime_ms.map(|time| time - 1);
        assert!(origin.projected_tail_position(&next, now).is_none());
        next.control_uptime_ms = clock.control_uptime_ms.map(|time| time + 801);
        assert!(origin.projected_tail_position(&next, now).is_none());
        next.control_uptime_ms = clock.control_uptime_ms.map(|time| time + 214);
        next.generation += 1;
        assert!(origin.projected_tail_position(&next, now).is_none());
        next.generation = origin.generation;
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, 4.01] {
            origin.rate = rate;
            assert!(
                origin.projected_tail_position(&next, now).is_none(),
                "{rate}"
            );
        }
    }

    #[test]
    fn successor_first_eof_warms_only_the_observed_native_queue_head() {
        let (mut core, slot, lease, clock) = natural_next_origin();
        observe_spatial_next_clock(&mut core, clock.clone());
        core.check_host_spatial_clock(&PlayerCommand::Remote {
            provider: lease.provider.clone(),
            action: RemoteAction::OutputQueue {
                queue: natural_successor_queue(&clock),
            },
        });
        let mut successor = natural_next_successor(clock);
        successor.state = PlaybackState::Loading;
        successor.rate = 0.0;
        successor.position_ms = 0;
        successor.actions = None;
        successor.metadata = None;
        observe_spatial_next_clock(&mut core, successor.clone());
        assert_eq!(
            slot.lock().unwrap().natural_next_warm,
            Some(lease.generation)
        );
        assert!(slot.lock().unwrap().lease.is_none());

        successor.sequence += 1;
        successor.queue_epoch = successor.queue_epoch.map(|epoch| epoch + 1);
        successor.state = PlaybackState::Playing;
        successor.rate = 1.0;
        successor.actions = Some(Default::default());
        successor.metadata = Some(RemoteOutputMetadata {
            title: "Exact queued successor".into(),
            artists: vec!["Fixture".into()],
            album: None,
            duration_ms: NATURAL_TEST_DURATION,
            artwork: None,
            context_uri: Some(NATURAL_TEST_CONTEXT.into()),
        });
        observe_spatial_next_clock(&mut core, successor.clone());
        let replacement = accept_natural_successor_lease(&mut core, lease, &successor);
        let slot = slot.lock().unwrap();
        assert_eq!(slot.lease.as_ref(), Some(&replacement));
        assert_eq!(slot.natural_next_warm, Some(replacement.generation - 1));
    }

    #[test]
    fn actual_successor_clock_then_host_retirement_keeps_only_the_proven_natural_warmer() {
        for full_revoke in [false, true] {
            let (mut core, slot, lease, clock) = natural_next_origin();
            core.send(PlayerCommand::Remote {
                provider: lease.provider.clone(),
                action: RemoteAction::OutputClock {
                    clock: Some(clock.clone()),
                },
            })
            .unwrap();
            core.send(PlayerCommand::Remote {
                provider: lease.provider.clone(),
                action: RemoteAction::OutputQueue {
                    queue: natural_successor_queue(&clock),
                },
            })
            .unwrap();
            let mut successor = natural_next_successor(clock);
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            successor.actions = None;
            successor.metadata = None;
            // Exercise execute/remote routing, not just the clock helper: Swift
            // forwards the new raw marker before retiring its old local lease.
            core.send(PlayerCommand::Remote {
                provider: lease.provider.clone(),
                action: RemoteAction::OutputClock {
                    clock: Some(successor.clone()),
                },
            })
            .unwrap();
            assert!(core.host_analysis.warming_next.is_some());
            if full_revoke {
                core.send(PlayerCommand::HostAnalysis { lease: None })
                    .unwrap();
                assert!(core.host_analysis.warming_next.is_none());
                assert_eq!(slot.lock().unwrap().natural_next_warm, None);
            } else {
                let deadline = core.host_analysis.warming_next.as_ref().unwrap().deadline;
                core.send(PlayerCommand::RetireHostAnalysis {
                    generation: lease.generation,
                })
                .unwrap();
                assert_eq!(
                    core.host_analysis.warming_next.as_ref().unwrap().deadline,
                    deadline
                );
                assert!(
                    slot.lock().unwrap().lease.is_none(),
                    "retirement cannot acquire a capture lease"
                );
                successor.sequence += 1;
                successor.state = PlaybackState::Playing;
                successor.rate = 1.0;
                successor.actions = Some(Default::default());
                successor.metadata = Some(RemoteOutputMetadata {
                    title: "Confirmed successor".into(),
                    artists: vec![],
                    album: None,
                    artwork: None,
                    duration_ms: NATURAL_TEST_DURATION,
                    context_uri: Some(NATURAL_TEST_CONTEXT.into()),
                });
                observe_spatial_next_clock(&mut core, successor.clone());
                let replacement =
                    accept_natural_successor_lease(&mut core, lease.clone(), &successor);
                core.send(PlayerCommand::RetireHostAnalysis {
                    generation: lease.generation,
                })
                .unwrap();
                assert_eq!(
                    slot.lock().unwrap().lease.as_ref(),
                    Some(&replacement),
                    "a late old release cannot revoke the new route"
                );
                assert_eq!(
                    slot.lock().unwrap().natural_next_warm,
                    Some(lease.generation)
                );
            }
        }
    }

    #[test]
    fn scoped_host_retirement_never_preserves_an_invalid_or_expired_warmer() {
        for missing in [
            "expired",
            "stale clock",
            "auth",
            "external device",
            "inactive device",
            "restricted device",
            "lost observation",
            "wrong provider",
        ] {
            let (mut core, slot, lease, clock) = natural_next_origin();
            observe_spatial_next_clock(&mut core, clock.clone());
            core.observe_spatial_successor_queue(&lease.provider, &natural_successor_queue(&clock));
            let mut successor = natural_next_successor(clock);
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            successor.actions = None;
            successor.metadata = None;
            observe_spatial_next_clock(&mut core, successor);
            assert!(core.host_analysis.warming_next.is_some());
            match missing {
                "expired" => {
                    core.host_analysis.warming_next.as_mut().unwrap().deadline = Instant::now()
                }
                "stale clock" => {
                    core.host_analysis.output_clock.as_mut().unwrap().observed -=
                        LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "auth" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .authentication_required = true
                }
                "lost observation" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .owned_output_observed = false
                }
                "wrong provider" => core.remote.active = Some("other".into()),
                kind => {
                    let device = core
                        .remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .playback
                        .as_mut()
                        .unwrap()
                        .device
                        .as_mut()
                        .unwrap();
                    match kind {
                        "external device" => device.id = "external".into(),
                        "inactive device" => device.is_active = false,
                        "restricted device" => device.is_restricted = true,
                        _ => unreachable!(),
                    }
                }
            }
            core.retire_host_analysis_generation(lease.generation)
                .unwrap();
            assert!(core.host_analysis.warming_next.is_none(), "{missing}");
            assert!(slot.lock().unwrap().lease.is_none(), "{missing}");
            assert_eq!(slot.lock().unwrap().natural_next_warm, None, "{missing}");
        }
    }

    #[test]
    fn successor_first_eof_rejects_unproven_queue_tail_and_occurrence() {
        for missing in [
            "queue",
            "head",
            "queue epoch",
            "queue runtime",
            "queue device",
            "stale queue",
            "tail",
            "full origin",
            "new epoch",
            "context",
            "same URI",
        ] {
            let (mut core, slot, lease, mut clock) = natural_next_origin();
            if missing == "tail" {
                clock.position_ms -= 5_000;
            }
            if missing == "full origin" {
                clock.actions = None;
            }
            observe_spatial_next_clock(&mut core, clock.clone());
            let mut queue = natural_successor_queue(&clock);
            match missing {
                "head" => {
                    queue.upcoming[0].media_id = MediaId::parse("fixture:track:other").unwrap()
                }
                "queue epoch" => queue.epoch += 1,
                "queue runtime" => queue.generation += 1,
                "queue device" => queue.device_id = "foreign".into(),
                _ => {}
            }
            if missing != "queue" {
                core.check_host_spatial_clock(&PlayerCommand::Remote {
                    provider: lease.provider.clone(),
                    action: RemoteAction::OutputQueue { queue },
                });
            }
            if missing == "stale queue" {
                core.host_analysis
                    .successor_queue
                    .as_mut()
                    .unwrap()
                    .observed -= SPATIAL_QUEUE_FRESHNESS + Duration::from_millis(1);
            }
            let mut successor = natural_next_successor(clock.clone());
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            successor.actions = None;
            successor.metadata = None;
            match missing {
                "new epoch" => successor.queue_epoch = clock.queue_epoch,
                "same URI" => successor.media_id = clock.media_id,
                "context" => {
                    successor.metadata = Some(RemoteOutputMetadata {
                        title: "Foreign context".into(),
                        artists: vec![],
                        album: None,
                        duration_ms: NATURAL_TEST_DURATION,
                        artwork: None,
                        context_uri: Some("fixture:album:foreign".into()),
                    })
                }
                _ => {}
            }
            observe_spatial_next_clock(&mut core, successor);
            assert!(core.host_analysis.warming_next.is_none(), "{missing}");
            assert_eq!(slot.lock().unwrap().natural_next_warm, None, "{missing}");
            assert_eq!(
                slot.lock().unwrap().lease.as_ref(),
                Some(&lease),
                "{missing}"
            );
        }
    }

    #[test]
    fn empty_native_queue_revokes_head_and_replay_cannot_restore_it() {
        for transport_clear in [false, true] {
            let (mut core, slot, lease, clock) = natural_next_origin();
            observe_spatial_next_clock(&mut core, clock.clone());
            let original = natural_successor_queue(&clock);
            core.observe_spatial_successor_queue(&lease.provider, &original);
            assert!(core.host_analysis.successor_queue.is_some());
            let mut empty = original.clone();
            empty.sequence += 1;
            empty.upcoming.clear();
            core.observe_spatial_successor_queue(&lease.provider, &empty);
            if transport_clear {
                core.clear_host_analysis();
                let mut replacement = lease.clone();
                replacement.generation += 1;
                observe_spatial_next_clock(&mut core, clock.clone());
                core.update_host_analysis(Some(replacement)).unwrap();
                core.poll_host_analysis();
            }
            core.observe_spatial_successor_queue(&lease.provider, &original);
            assert!(core.host_analysis.successor_queue.is_none());
            let mut successor = natural_next_successor(clock);
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            observe_spatial_next_clock(&mut core, successor);
            assert!(core.host_analysis.warming_next.is_none());
            assert_eq!(slot.lock().unwrap().natural_next_warm, None);
        }
    }

    fn accept_natural_successor_lease(
        core: &mut PlaybackCoordinator,
        mut lease: HostAnalysisLease,
        clock: &RemoteOutputClock,
    ) -> HostAnalysisLease {
        // The ordinary provider projection and host then acquire this playing item.
        // Even when an earlier warm proof failed, normal playback stays available.
        lease.generation += 1;
        lease.media_id = clock.media_id.clone().unwrap();
        lease.device_id = clock.device_id.clone();
        let playback = core
            .remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.current.as_mut().unwrap().id = lease.media_id.clone();
        playback.state = PlaybackState::Playing;
        playback.position_ms = clock.position_ms;
        playback.position_rate = 1.0;
        playback.device.as_mut().unwrap().id = lease.device_id.clone();
        playback.context_uri = clock
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.context_uri.clone());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();
        lease
    }

    #[test]
    fn natural_eof_retires_old_capture_and_warms_the_exact_album_successor() {
        for epoch_changes_after_full_state in [false, true] {
            let (mut core, slot, lease, clock) = natural_next_origin();
            observe_spatial_next_clock(&mut core, clock.clone());
            let mut loading = natural_next_loading(clock);
            let complete_actions = loading.actions.take();
            observe_spatial_next_clock(&mut core, loading.clone());
            assert_eq!(core.host_analysis_revoked_through(), Some(lease.generation));
            {
                let slot = slot.lock().unwrap();
                assert_eq!(slot.natural_next_warm, Some(lease.generation));
                assert!(slot.lease.is_none());
            }
            core.update_host_analysis(Some(lease.clone())).unwrap();
            assert!(slot.lock().unwrap().lease.is_none());

            let mut successor = natural_next_successor(loading);
            let context = successor.metadata.as_mut().unwrap().context_uri.take();
            // track_changed opens a provisional epoch with no context or actions.
            successor.state = PlaybackState::Loading;
            successor.rate = 0.0;
            successor.position_ms = 0;
            observe_spatial_next_clock(&mut core, successor.clone());
            assert!(core.host_analysis.warming_next.is_some());
            assert!(slot.lock().unwrap().lease.is_none());

            // The first full state recovers the album context and advances the epoch again.
            successor.sequence += 1;
            successor.queue_epoch = successor.queue_epoch.map(|epoch| epoch + 1);
            successor.actions = complete_actions;
            successor.metadata.as_mut().unwrap().context_uri = context;
            successor.state = PlaybackState::Playing;
            successor.rate = 1.0;
            successor.position_ms = 1;
            observe_spatial_next_clock(&mut core, successor.clone());
            assert!(core.host_analysis.warming_next.is_some());

            if epoch_changes_after_full_state {
                successor.sequence += 1;
                successor.queue_epoch = successor.queue_epoch.map(|epoch| epoch + 1);
                observe_spatial_next_clock(&mut core, successor.clone());
                assert!(core.host_analysis.warming_next.is_none());
            }

            let generation = lease.generation;
            let replacement = accept_natural_successor_lease(&mut core, lease, &successor);
            assert!(core.host_analysis.warming_next.is_none());
            let slot = slot.lock().unwrap();
            assert_eq!(slot.lease.as_ref(), Some(&replacement));
            assert_eq!(
                slot.natural_next_warm,
                (!epoch_changes_after_full_state).then_some(generation)
            );
        }
    }

    #[test]
    fn natural_eof_requires_an_active_fresh_owned_origin_near_the_album_end() {
        for missing in [
            "moving proof",
            "stale origin",
            "near end",
            "scope",
            "active relay",
            "repeat track",
        ] {
            let (mut core, slot, lease, mut clock) = natural_next_origin();
            if missing == "moving proof" {
                clock.actions = None;
            }
            if missing == "active relay" {
                slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Starting);
            }
            observe_spatial_next_clock(&mut core, clock.clone());
            let mut loading = natural_next_loading(clock);
            match missing {
                "stale origin" => {
                    core.host_analysis.output_clock.as_mut().unwrap().observed -=
                        LEASE_FRESHNESS + Duration::from_millis(1);
                }
                "near end" => loading.position_ms -= 5_000,
                "scope" => loading.metadata.as_mut().unwrap().context_uri = None,
                "repeat track" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .playback
                        .as_mut()
                        .unwrap()
                        .repeat = RepeatMode::One;
                }
                _ => {}
            }
            observe_spatial_next_clock(&mut core, loading);
            assert!(core.host_analysis.warming_next.is_none(), "{missing}");
            let slot = slot.lock().unwrap();
            assert_eq!(slot.natural_next_warm, None, "{missing}");
            assert_eq!(slot.lease.as_ref(), Some(&lease), "{missing}");
        }
    }

    #[test]
    fn natural_eof_discards_the_warmer_after_competing_transport_or_invalid_successor() {
        for interruption in [
            "origin resumed",
            "explicit pause",
            "different context",
            "different device",
            "different runtime",
            "different process",
            "same URI",
            "same occurrence",
            "second successor",
            "incomplete successor",
            "expired",
        ] {
            let (mut core, slot, mut lease, clock) = natural_next_origin();
            observe_spatial_next_clock(&mut core, clock.clone());
            let loading = natural_next_loading(clock.clone());
            observe_spatial_next_clock(&mut core, loading.clone());
            assert_eq!(
                slot.lock().unwrap().natural_next_warm,
                Some(lease.generation)
            );

            let mut successor = natural_next_successor(loading);
            match interruption {
                "origin resumed" => {
                    let mut resumed = clock.clone();
                    resumed.sequence = successor.sequence;
                    observe_spatial_next_clock(&mut core, resumed);
                    successor.sequence += 1;
                }
                "explicit pause" => core.send(PlayerCommand::Pause).unwrap(),
                "different context" => {
                    successor.metadata.as_mut().unwrap().context_uri =
                        Some("spotify:album:1ATL5GLyefJaxhQzSPVrLX".into());
                }
                "different device" => successor.device_id = "replacement".into(),
                "different runtime" => successor.generation += 1,
                "different process" => lease.process_id += 1,
                "same URI" => successor.media_id = clock.media_id,
                "same occurrence" => successor.queue_epoch = clock.queue_epoch,
                "second successor" => {
                    let mut first = successor.clone();
                    first.state = PlaybackState::Loading;
                    first.position_ms = 0;
                    first.rate = 0.0;
                    observe_spatial_next_clock(&mut core, first);
                    assert!(core.host_analysis.warming_next.is_some());
                    successor.sequence += 1;
                    successor.media_id = Some(MediaId::parse("fixture:track:other-next").unwrap());
                }
                "incomplete successor" => successor.actions = None,
                "expired" => {
                    core.host_analysis.warming_next.as_mut().unwrap().deadline = Instant::now();
                }
                _ => unreachable!(),
            }
            observe_spatial_next_clock(&mut core, successor.clone());
            let replacement = accept_natural_successor_lease(&mut core, lease, &successor);
            assert!(core.host_analysis.warming_next.is_none(), "{interruption}");
            let slot = slot.lock().unwrap();
            assert_eq!(slot.natural_next_warm, None, "{interruption}");
            assert_eq!(slot.lease.as_ref(), Some(&replacement), "{interruption}");
        }
    }

    #[test]
    fn dormant_renderer_requires_fresh_full_playing_then_same_stationary_occurrence() {
        for scope in [
            "exact",
            "no moving",
            "aged moving",
            "incomplete moved",
            "epoch",
            "runtime",
            "no control",
        ] {
            let (mut core, slot, mut lease) = setup();
            let state = core.remote.states.get_mut(&lease.provider).unwrap();
            state.owned_output_control = scope != "no control";
            lease.spatial_stereo = true;
            let mut paused = paused_output_clock(&lease);
            paused.actions = Some(Default::default());
            let mut playing = paused.clone();
            playing.state = PlaybackState::Playing;
            playing.rate = 1.0;
            if scope != "no moving" {
                observe_spatial_next_clock(&mut core, playing);
            }
            // Ownership can be acquired just after the full Playing clock arrives.
            core.update_host_analysis(Some(lease.clone())).unwrap();
            core.poll_analysis();
            assert_eq!(slot.lock().unwrap().confirmed_renderer_only_pause, None);

            paused.sequence += 1;
            match scope {
                "aged moving" => {
                    core.host_analysis
                        .renderer_only_pause
                        .as_mut()
                        .unwrap()
                        .playing_observed -= LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "incomplete moved" => paused.actions = None,
                "epoch" => paused.queue_epoch = Some(4),
                "runtime" => paused.generation += 1,
                _ => {}
            }
            let playback = core
                .remote
                .states
                .get_mut(&lease.provider)
                .unwrap()
                .playback
                .as_mut()
                .unwrap();
            playback.state = PlaybackState::Paused;
            playback.position_rate = 0.0;
            lease.playing = false;
            core.update_host_analysis(Some(lease.clone())).unwrap();
            observe_spatial_next_clock(&mut core, paused.clone());
            let previous_publications = slot.lock().unwrap().pause_publications;
            core.poll_analysis();
            let state = slot.lock().unwrap();
            assert_eq!(
                state.pause_publications,
                previous_publications + 1,
                "{scope}: polling publishes the lease and both pause proofs through the batch override"
            );
            assert_eq!(state.confirmed_pause, Some(lease.generation), "{scope}");
            assert_eq!(
                state.confirmed_renderer_only_pause,
                (scope == "exact").then_some(lease.generation),
                "{scope}"
            );
            drop(state);

            if scope == "exact" {
                // A new paused heartbeat preserves the proved transition after the moving clock ages.
                core.host_analysis
                    .renderer_only_pause
                    .as_mut()
                    .unwrap()
                    .playing_observed -= LEASE_FRESHNESS;
                paused.sequence += 1;
                observe_spatial_next_clock(&mut core, paused.clone());
                core.poll_analysis();
                assert_eq!(
                    slot.lock().unwrap().confirmed_renderer_only_pause,
                    Some(lease.generation)
                );
                paused.sequence += 1;
                paused.active = false;
                observe_spatial_next_clock(&mut core, paused);
                core.poll_analysis();
                assert_eq!(slot.lock().unwrap().confirmed_renderer_only_pause, None);
            }
        }
    }

    fn confirmed_renderer_pause() -> (
        PlaybackCoordinator,
        Arc<Mutex<Slot>>,
        HostAnalysisLease,
        RemoteOutputClock,
    ) {
        let (mut core, slot, mut lease) = setup();
        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .owned_output_control = true;
        lease.spatial_stereo = true;
        let mut clock = paused_output_clock(&lease);
        clock.actions = Some(Default::default());
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        observe_spatial_next_clock(&mut core, clock.clone());
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();

        let playback = core
            .remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.state = PlaybackState::Paused;
        playback.position_rate = 0.0;
        lease.playing = false;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        clock.sequence += 1;
        clock.state = PlaybackState::Paused;
        clock.rate = 0.0;
        observe_spatial_next_clock(&mut core, clock.clone());
        core.poll_host_analysis();
        assert_eq!(
            slot.lock().unwrap().confirmed_renderer_only_pause,
            Some(lease.generation)
        );
        (core, slot, lease, clock)
    }

    #[test]
    fn playing_clock_before_host_resume_keeps_only_the_confirmed_paused_snapshot() {
        let (mut core, slot, mut lease, mut clock) = confirmed_renderer_pause();
        let observed = core.host_analysis.lease.as_ref().unwrap().1;
        let publications = slot.lock().unwrap().pause_publications;
        let playback = core
            .remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.state = PlaybackState::Playing;
        playback.position_rate = 1.0;
        clock.sequence += 1;
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        observe_spatial_next_clock(&mut core, clock);

        // Runtime polls between the raw output clock and the separate host lease.
        // Retain stopped resources, without authorizing audio or renewing the lease.
        assert!(core.poll_host_analysis().is_none());
        {
            let state = slot.lock().unwrap();
            assert_eq!(state.lease.as_ref(), Some(&lease));
            assert_eq!(state.confirmed_pause, Some(lease.generation));
            assert_eq!(state.confirmed_renderer_only_pause, Some(lease.generation));
            assert_eq!(state.pause_publications, publications);
        }
        assert_eq!(core.host_analysis.lease.as_ref().unwrap().1, observed);
        assert!(
            core.host_analysis
                .renderer_only_pause
                .as_ref()
                .unwrap()
                .confirmed
        );

        lease.playing = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();
        let state = slot.lock().unwrap();
        assert_eq!(state.lease.as_ref(), Some(&lease));
        assert_eq!(state.confirmed_pause, None);
        assert_eq!(state.confirmed_renderer_only_pause, None);
        assert_eq!(state.pause_publications, publications + 1);
    }

    #[test]
    fn explicit_play_partial_zero_clock_keeps_the_paused_graph_until_full_host_confirmation() {
        let (mut core, slot, mut lease, mut clock) = confirmed_renderer_pause();
        // The most recent paused heartbeat can be partial, long after its full
        // stationary pause was confirmed. It must preserve that existing proof.
        for _ in 0..3 {
            clock.sequence += 1;
            clock.actions = None;
            observe_spatial_next_clock(&mut core, clock.clone());
            core.poll_host_analysis();
            assert_eq!(
                slot.lock().unwrap().confirmed_renderer_only_pause,
                Some(lease.generation)
            );
        }
        let observed = core.host_analysis.lease.as_ref().unwrap().1;
        let publications = slot.lock().unwrap().pause_publications;
        assert_eq!(
            core.preserve_host_spatial_transport(&PlayerCommand::Play),
            Some(PlaybackState::Playing)
        );
        let deadline = core
            .host_analysis
            .resume_clock_gap
            .as_ref()
            .unwrap()
            .deadline;

        // playback_changed(playing) keeps the old paused rate and clears actions.
        // The complete snapshot and the separate Swift host lease follow later.
        clock.sequence += 1;
        clock.state = PlaybackState::Playing;
        clock.actions = None;
        let playback = core
            .remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.state = PlaybackState::Playing;
        playback.position_rate = 0.0;
        observe_spatial_next_clock(&mut core, clock.clone());
        for _ in 0..3 {
            assert!(core.poll_host_analysis().is_none());
            let state = slot.lock().unwrap();
            assert_eq!(state.lease.as_ref(), Some(&lease));
            assert_eq!(state.confirmed_pause, Some(lease.generation));
            assert_eq!(state.confirmed_renderer_only_pause, Some(lease.generation));
            assert_eq!(state.pause_publications, publications);
        }
        assert_eq!(core.host_analysis.lease.as_ref().unwrap().1, observed);
        assert_eq!(
            core.host_analysis
                .resume_clock_gap
                .as_ref()
                .unwrap()
                .deadline,
            deadline
        );

        clock.sequence += 1;
        clock.rate = 1.0;
        clock.actions = Some(Default::default());
        // The full raw clock can also precede the provider's positive-rate
        // projection, independently of the later host lease publication.
        observe_spatial_next_clock(&mut core, clock.clone());
        core.poll_host_analysis();
        assert_eq!(slot.lock().unwrap().pause_publications, publications);
        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .position_rate = 1.0;
        observe_spatial_next_clock(&mut core, clock);
        core.poll_host_analysis();
        assert_eq!(slot.lock().unwrap().pause_publications, publications);
        lease.playing = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();
        let state = slot.lock().unwrap();
        assert_eq!(state.lease.as_ref(), Some(&lease));
        assert_eq!(state.confirmed_pause, None);
        assert_eq!(state.confirmed_renderer_only_pause, None);
        assert_eq!(state.pause_publications, publications + 1);
        assert!(core.host_analysis.resume_clock_gap.is_none());
    }

    #[test]
    fn full_playing_clock_before_the_owned_provider_projection_keeps_only_the_paused_source() {
        let (mut core, slot, mut lease, mut clock) = confirmed_renderer_pause();
        core.preserve_host_spatial_transport(&PlayerCommand::Play);
        let publications = slot.lock().unwrap().pause_publications;
        clock.sequence += 1;
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        observe_spatial_next_clock(&mut core, clock);
        core.poll_host_analysis();
        {
            let state = slot.lock().unwrap();
            assert_eq!(state.lease.as_ref(), Some(&lease));
            assert_eq!(state.confirmed_renderer_only_pause, Some(lease.generation));
            assert_eq!(state.pause_publications, publications);
        }
        let playback = core
            .remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.state = PlaybackState::Playing;
        playback.position_rate = 1.0;
        core.poll_host_analysis();
        assert_eq!(slot.lock().unwrap().pause_publications, publications);
        lease.playing = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_host_analysis();
        let state = slot.lock().unwrap();
        assert_eq!(state.lease.as_ref(), Some(&lease));
        assert_eq!(state.confirmed_renderer_only_pause, None);
        assert_eq!(state.pause_publications, publications + 1);
    }

    #[test]
    fn stationary_partial_can_arm_resume_only_with_the_existing_current_pause_proof() {
        for invalid in [
            "missing proof",
            "unconfirmed proof",
            "device",
            "media",
            "runtime",
            "epoch",
            "missing epoch",
            "stale clock",
            "expired lease",
            "unowned",
            "inactive",
        ] {
            let (mut core, _slot, lease, mut clock) = confirmed_renderer_pause();
            clock.sequence += 1;
            clock.actions = None;
            match invalid {
                "missing proof" => core.host_analysis.renderer_only_pause = None,
                "unconfirmed proof" => {
                    core.host_analysis
                        .renderer_only_pause
                        .as_mut()
                        .unwrap()
                        .confirmed = false
                }
                "device" => clock.device_id = "foreign-output".into(),
                "media" => clock.media_id = Some(MediaId::parse("fixture:track:other").unwrap()),
                "runtime" => clock.generation += 1,
                "epoch" => clock.queue_epoch = clock.queue_epoch.map(|epoch| epoch + 1),
                "missing epoch" => clock.queue_epoch = None,
                "expired lease" => {
                    core.host_analysis.lease.as_mut().unwrap().1 -=
                        LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "unowned" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .owned_output_control = false
                }
                "inactive" => clock.active = false,
                _ => {}
            }
            observe_spatial_next_clock(&mut core, clock);
            if invalid == "stale clock" {
                core.host_analysis.output_clock.as_mut().unwrap().observed -=
                    LEASE_FRESHNESS + Duration::from_millis(1);
            }
            // Normal poll also applies the unchanged ownership/expiry rules
            // before an explicit Play reaches the retention admission path.
            core.poll_host_analysis();
            core.preserve_host_spatial_transport(&PlayerCommand::Play);
            assert!(core.host_analysis.resume_clock_gap.is_none(), "{invalid}");
        }
    }

    #[test]
    fn partial_zero_resume_gap_cannot_keep_an_unrequested_stale_or_changed_output() {
        for invalid in [
            "no play",
            "full moving no play",
            "device",
            "media",
            "runtime",
            "epoch",
            "missing epoch",
            "context",
            "stale clock",
            "expired lease",
            "expired intent",
            "expired intent full",
            "pause intent",
            "missing proof",
            "unowned",
            "new pause",
            "cancelled play",
            "complete zero playing",
        ] {
            let (mut core, slot, lease, mut clock) = confirmed_renderer_pause();
            if !matches!(invalid, "no play" | "full moving no play") {
                assert_eq!(
                    core.preserve_host_spatial_transport(&PlayerCommand::Play),
                    Some(PlaybackState::Playing)
                );
            }
            clock.sequence += 1;
            clock.state = PlaybackState::Playing;
            clock.actions = None;
            core.remote
                .states
                .get_mut(&lease.provider)
                .unwrap()
                .playback
                .as_mut()
                .unwrap()
                .state = PlaybackState::Playing;
            match invalid {
                "device" => clock.device_id = "foreign-output".into(),
                "media" => clock.media_id = Some(MediaId::parse("fixture:track:other").unwrap()),
                "runtime" => clock.generation += 1,
                "epoch" => clock.queue_epoch = clock.queue_epoch.map(|epoch| epoch + 1),
                "missing epoch" => clock.queue_epoch = None,
                "context" => {
                    clock.metadata = Some(RemoteOutputMetadata {
                        title: "Original owned fixture".into(),
                        artists: vec![],
                        album: None,
                        duration_ms: 10_000,
                        artwork: None,
                        context_uri: Some(NATURAL_TEST_CONTEXT.into()),
                    })
                }
                "expired lease" => {
                    core.host_analysis.lease.as_mut().unwrap().1 -=
                        LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "expired intent" | "expired intent full" => {
                    core.host_analysis
                        .resume_clock_gap
                        .as_mut()
                        .unwrap()
                        .deadline = Instant::now()
                }
                "pause intent" => {
                    core.host_analysis.pause_pending_until = Some(Instant::now() + LEASE_FRESHNESS)
                }
                "missing proof" => core.host_analysis.renderer_only_pause = None,
                "unowned" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .owned_output_control = false
                }
                "new pause" => {
                    core.preserve_host_spatial_transport(&PlayerCommand::Pause);
                }
                "cancelled play" => core.cancel_spatial_resume_clock_gap(),
                "complete zero playing" => clock.actions = Some(Default::default()),
                _ => {}
            }
            if matches!(invalid, "full moving no play" | "expired intent full") {
                clock.actions = Some(Default::default());
                clock.rate = 1.0;
            }
            observe_spatial_next_clock(&mut core, clock);
            if invalid == "stale clock" {
                core.host_analysis.output_clock.as_mut().unwrap().observed -=
                    LEASE_FRESHNESS + Duration::from_millis(1);
            }
            assert!(core.poll_host_analysis().is_none(), "{invalid}");
            let state = slot.lock().unwrap();
            assert_eq!(state.confirmed_renderer_only_pause, None, "{invalid}");
            assert!(
                state.lease.as_ref().is_none_or(|lease| !lease.playing),
                "{invalid}"
            );
        }
    }

    #[test]
    fn waiting_for_host_resume_never_preserves_an_unproved_or_changed_output() {
        for invalid in [
            "missing proof",
            "unconfirmed proof",
            "clock device",
            "clock media",
            "runtime",
            "epoch",
            "missing epoch",
            "incomplete",
            "stale clock",
            "expired lease",
            "pause intent",
            "unowned",
        ] {
            let (mut core, slot, lease, mut clock) = confirmed_renderer_pause();
            let playback = core
                .remote
                .states
                .get_mut(&lease.provider)
                .unwrap()
                .playback
                .as_mut()
                .unwrap();
            playback.state = PlaybackState::Playing;
            playback.position_rate = 1.0;
            clock.sequence += 1;
            clock.state = PlaybackState::Playing;
            clock.rate = 1.0;
            match invalid {
                "missing proof" => core.host_analysis.renderer_only_pause = None,
                "unconfirmed proof" => {
                    core.host_analysis
                        .renderer_only_pause
                        .as_mut()
                        .unwrap()
                        .confirmed = false
                }
                "clock device" => clock.device_id = "external-output".into(),
                "clock media" => {
                    clock.media_id = Some(MediaId::parse("fixture:track:other").unwrap())
                }
                "runtime" => clock.generation += 1,
                "epoch" => clock.queue_epoch = clock.queue_epoch.map(|epoch| epoch + 1),
                "missing epoch" => clock.queue_epoch = None,
                "incomplete" => clock.actions = None,
                "expired lease" => {
                    core.host_analysis.lease.as_mut().unwrap().1 -=
                        LEASE_FRESHNESS + Duration::from_millis(1)
                }
                "pause intent" => {
                    core.host_analysis.pause_pending_until = Some(Instant::now() + LEASE_FRESHNESS)
                }
                "unowned" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .owned_output_control = false
                }
                _ => {}
            }
            observe_spatial_next_clock(&mut core, clock);
            if invalid == "stale clock" {
                core.host_analysis.output_clock.as_mut().unwrap().observed -=
                    LEASE_FRESHNESS + Duration::from_millis(1);
            }
            assert!(core.poll_host_analysis().is_none(), "{invalid}");
            let state = slot.lock().unwrap();
            assert_eq!(state.confirmed_renderer_only_pause, None, "{invalid}");
            assert!(
                state.lease.as_ref().is_none_or(|lease| !lease.playing),
                "{invalid}"
            );
        }
    }

    #[test]
    fn deferred_spatial_play_dispatches_only_the_first_request_after_renderer_ready() {
        let (mut core, slot, calls) = setup_paused_resume();
        for request_id in [42, 43] {
            core.send_request(CommandRequest {
                request_id,
                command: PlayerCommand::Play,
            });
        }
        assert!(calls.actions.lock().unwrap().is_empty());
        assert!(!calls.polled_owned_play.load(Ordering::Relaxed));
        slot.lock().unwrap().resume = SpatialResume::Ready;
        core.poll_deferred_spatial_play();
        core.poll_deferred_spatial_play();
        assert_eq!(calls.play_request_ids.lock().unwrap().as_slice(), [42]);
        assert!(calls.polled_owned_play.load(Ordering::Relaxed));
    }

    #[test]
    fn deferred_latest_play_survives_transient_playing_before_an_earlier_pause_settles() {
        let (mut core, slot, calls) = setup_paused_resume();
        slot.lock().unwrap().resume = SpatialResume::Ready;
        core.send_request(CommandRequest {
            request_id: 42,
            command: PlayerCommand::Play,
        });
        core.poll_deferred_spatial_play();
        core.send_request(CommandRequest {
            request_id: 43,
            command: PlayerCommand::Pause,
        });
        slot.lock().unwrap().resume = SpatialResume::Warming;
        core.send_request(CommandRequest {
            request_id: 44,
            command: PlayerCommand::Play,
        });
        assert_eq!(
            core.host_analysis
                .deferred_play
                .as_ref()
                .unwrap()
                .request_id,
            Some(44)
        );
        let relay = slot.lock().unwrap().lease.clone();
        let playback = core
            .remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.state = PlaybackState::Playing;
        playback.position_rate = 1.0;
        // The first Play became audible and discarded its old prewarm, while
        // the previous Pause still needs the latest explicit intent after it.
        slot.lock().unwrap().resume = SpatialResume::Unavailable;
        core.poll_deferred_spatial_play();
        core.poll_deferred_spatial_play();
        assert_eq!(calls.play_request_ids.lock().unwrap().as_slice(), [42, 44]);
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [
                RemoteAction::Control {
                    control: RemoteControl::Play
                },
                RemoteAction::Control {
                    control: RemoteControl::Pause
                },
                RemoteAction::Control {
                    control: RemoteControl::Play
                },
            ]
        ));
        assert!(core.host_analysis.deferred_play.is_none());
        assert_eq!(
            slot.lock().unwrap().lease,
            relay,
            "Already moving output keeps its independently owned relay"
        );
    }

    fn armed_playing_seek() -> (
        PlaybackCoordinator,
        Arc<Mutex<Slot>>,
        HostAnalysisLease,
        RemoteOutputClock,
    ) {
        let (mut core, slot, mut lease) = setup();
        let calls = Arc::new(ResumeCalls::default());
        core.register_remote(ResumeProvider(calls.clone()));
        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .owned_output_control = true;
        lease.spatial_stereo = true;
        core.update_host_analysis(Some(lease.clone())).unwrap();
        core.poll_analysis();

        let mut clock = paused_output_clock(&lease);
        clock.state = PlaybackState::Playing;
        clock.rate = 1.0;
        observe_spatial_next_clock(&mut core, clock.clone());
        core.send_request(CommandRequest {
            request_id: 42,
            command: PlayerCommand::Seek { position_ms: 8_000 },
        });
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Seek { position_ms: 8_000 }
            }]
        ));
        assert!(core.host_analysis.lease.is_none());
        assert!(core.host_analysis.warming_seek.is_some());
        assert_eq!(slot.lock().unwrap().seek_warm, Some(lease.generation));
        assert!(core.poll_host_analysis().is_none());
        (core, slot, lease, clock)
    }

    #[test]
    fn playing_seek_hands_off_only_for_the_exact_correlated_new_clock_and_lease() {
        for proof in [
            Some((42, 2)),
            Some((41, 2)),
            Some((42, 1)),
            Some((42, 3)),
            None,
        ] {
            let (mut core, slot, mut lease, mut clock) = armed_playing_seek();
            // An old heartbeat cannot revive the revoked capture or consume the warmer.
            core.update_host_analysis(Some(lease.clone())).unwrap();
            assert_eq!(slot.lock().unwrap().seek_warm, Some(1));
            clock.sequence = 2;
            clock.position_ms = 8_000;
            observe_spatial_next_clock(&mut core, clock);
            lease.generation += 1;
            let command = if let Some((request_id, sequence)) = proof {
                PlayerCommand::HostAnalysisAfterSeek {
                    lease: lease.clone(),
                    request_id,
                    sequence,
                }
            } else {
                PlayerCommand::HostAnalysis {
                    lease: Some(lease.clone()),
                }
            };
            core.send(command).unwrap();

            assert!(core.host_analysis.warming_seek.is_none());
            let slot = slot.lock().unwrap();
            assert_eq!(slot.lease.as_ref(), Some(&lease));
            assert_eq!(slot.seek_warm, (proof == Some((42, 2))).then_some(1));
        }
    }

    #[test]
    fn playing_seek_cancels_on_scope_changes_and_competing_transport() {
        for invalidate in [
            "occurrence",
            "runtime",
            "device",
            "pause",
            "auth",
            "deadline",
            "intent",
        ] {
            let (mut core, slot, mut lease, mut clock) = armed_playing_seek();
            clock.sequence = 2;
            match invalidate {
                "occurrence" => clock.queue_epoch = Some(4),
                "runtime" => clock.generation += 1,
                "device" => clock.device_id = "external".into(),
                "pause" => {
                    clock.state = PlaybackState::Paused;
                    clock.rate = 0.0;
                }
                "auth" => {
                    core.remote
                        .states
                        .get_mut(&lease.provider)
                        .unwrap()
                        .authentication_required = true
                }
                "deadline" => {
                    core.host_analysis.warming_seek.as_mut().unwrap().deadline = Instant::now()
                }
                "intent" => core.send(PlayerCommand::Pause).unwrap(),
                _ => unreachable!(),
            }
            observe_spatial_next_clock(&mut core, clock);
            core.revoke_stale_host_analysis();

            assert!(core.host_analysis.warming_seek.is_none(), "{invalidate}");
            assert!(slot.lock().unwrap().seek_warm.is_none(), "{invalidate}");
            lease.generation += 1;
            core.send(PlayerCommand::HostAnalysisAfterSeek {
                lease,
                request_id: 42,
                sequence: 2,
            })
            .unwrap();
            assert!(slot.lock().unwrap().seek_warm.is_none(), "{invalidate}");
        }
    }

    #[test]
    fn confirmed_paused_next_survives_stationary_clock_but_revokes_on_a_different_pause() {
        let (mut core, slot, calls) = setup_paused_resume();
        slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Paused);
        let lease = slot.lock().unwrap().lease.clone().unwrap();
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .transport
            .next = true;
        let mut clock = paused_output_clock(&lease);
        observe_spatial_next_clock(&mut core, clock.clone());

        core.send(PlayerCommand::Next).unwrap();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Next
            }]
        ));
        {
            let slot = slot.lock().unwrap();
            assert_eq!(slot.next_warm, Some(lease.generation));
            assert_eq!(slot.confirmed_pause, None);
            assert!(slot.lease.is_none());
        }
        clock.sequence += 1;
        observe_spatial_next_clock(&mut core, clock.clone());
        core.poll_analysis();
        assert_eq!(slot.lock().unwrap().next_warm, Some(lease.generation));

        clock.sequence += 1;
        clock.queue_epoch = Some(4);
        observe_spatial_next_clock(&mut core, clock);
        assert_eq!(slot.lock().unwrap().next_warm, None);
    }

    #[test]
    fn paused_next_with_stale_stationary_clock_dispatches_without_reusing_renderer() {
        let (mut core, slot, calls) = setup_paused_resume();
        slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Paused);
        let lease = slot.lock().unwrap().lease.clone().unwrap();
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .transport
            .next = true;
        observe_spatial_next_clock(&mut core, paused_output_clock(&lease));
        core.host_analysis.output_clock.as_mut().unwrap().observed -=
            LEASE_FRESHNESS + Duration::from_millis(1);

        core.send(PlayerCommand::Next).unwrap();
        assert_eq!(slot.lock().unwrap().next_warm, None);
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Next
            }]
        ));
    }

    #[test]
    fn same_output_paused_seek_preserves_suspended_relay_before_prepared_play() {
        let (mut core, slot, calls) = setup_paused_resume();
        slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Paused);
        let lease = slot.lock().unwrap().lease.clone().unwrap();

        core.send(PlayerCommand::Seek { position_ms: 1900 })
            .unwrap();
        assert_eq!(
            slot.lock().unwrap().resume_invalidations,
            [lease.generation]
        );
        assert_eq!(core.host_analysis_revoked_through(), None);
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert!(!core.host_visuals_are_current());

        core.send(PlayerCommand::Play).unwrap();
        core.poll_deferred_spatial_play();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Seek { position_ms: 1900 }
            }]
        ));
        slot.lock().unwrap().resume = SpatialResume::Ready;
        core.poll_deferred_spatial_play();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [
                RemoteAction::Control {
                    control: RemoteControl::Seek { position_ms: 1900 }
                },
                RemoteAction::Control {
                    control: RemoteControl::Play
                },
            ]
        ));
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert_eq!(slot.lock().unwrap().confirmed_pause, Some(lease.generation));

        core.remote
            .states
            .get_mut(&lease.provider)
            .unwrap()
            .owned_output_observed = false;
        core.publish_snapshot();
        assert_eq!(core.host_analysis_revoked_through(), Some(lease.generation));
        assert!(slot.lock().unwrap().lease.is_none());
        assert_eq!(slot.lock().unwrap().confirmed_pause, None);
    }

    #[test]
    fn each_paused_seek_retires_only_the_retained_renderer_clock() {
        let (mut core, slot, calls) = setup_paused_resume();
        slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Paused);
        let lease = slot.lock().unwrap().lease.clone().unwrap();
        for position_ms in [1_000, 2_000] {
            core.send(PlayerCommand::Seek { position_ms }).unwrap();
        }
        core.send(PlayerCommand::Remote {
            provider: lease.provider.clone(),
            action: RemoteAction::Control {
                control: RemoteControl::Seek { position_ms: 3_000 },
            },
        })
        .unwrap();
        assert_eq!(
            slot.lock().unwrap().resume_invalidations,
            [lease.generation; 3]
        );
        assert_eq!(slot.lock().unwrap().lease.as_ref(), Some(&lease));
        assert_eq!(core.host_analysis_revoked_through(), None);
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [
                RemoteAction::Control {
                    control: RemoteControl::Seek { position_ms: 1_000 }
                },
                RemoteAction::Control {
                    control: RemoteControl::Seek { position_ms: 2_000 }
                },
                RemoteAction::Control {
                    control: RemoteControl::Seek { position_ms: 3_000 }
                },
            ]
        ));
    }

    #[test]
    fn seek_clears_spatial_when_suspension_or_its_bound_playback_is_unproven() {
        for condition in [
            "starting",
            "loading",
            "moving",
            "media",
            "device",
            "no control",
            "deferred play",
            "pending navigation",
            "stale",
            "wrong provider",
        ] {
            let (mut core, slot, calls) = setup_paused_resume();
            slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Paused);
            let state = core.remote.states.get_mut("fixture").unwrap();
            let frame = state.playback.as_mut().unwrap();
            match condition {
                "starting" => {
                    slot.lock().unwrap().spatial_status = Some(HostSpatialStatus::Starting)
                }
                "loading" => frame.state = PlaybackState::Loading,
                "moving" => frame.position_rate = 1.0,
                "media" => {
                    frame.current.as_mut().unwrap().id =
                        MediaId::parse("fixture:track:other").unwrap()
                }
                "device" => frame.device.as_mut().unwrap().id = "other-device".into(),
                "no control" => state.owned_output_control = false,
                "deferred play" => core.send(PlayerCommand::Play).unwrap(),
                "pending navigation" => calls.pending_start.store(true, Ordering::Relaxed),
                "stale" => core.host_analysis.lease.as_mut().unwrap().1 -= LEASE_FRESHNESS,
                "wrong provider" => (),
                _ => unreachable!(),
            }
            let command = if condition == "wrong provider" {
                PlayerCommand::Remote {
                    provider: "another".into(),
                    action: RemoteAction::Control {
                        control: RemoteControl::Seek { position_ms: 1900 },
                    },
                }
            } else {
                PlayerCommand::Seek { position_ms: 1900 }
            };
            let _ = core.send(command);
            assert_eq!(core.host_analysis_revoked_through(), Some(1), "{condition}");
            assert!(slot.lock().unwrap().lease.is_none(), "{condition}");
            assert!(
                slot.lock().unwrap().resume_invalidations.is_empty(),
                "{condition}"
            );
            assert!(core.host_analysis.deferred_play.is_none(), "{condition}");
        }
    }

    #[test]
    fn prepared_resume_dispatches_once_and_keeps_media_paused_until_provider_proof() {
        let (mut core, slot, calls) = setup_paused_resume();
        core.send(PlayerCommand::Play).unwrap();
        core.send(PlayerCommand::Play).unwrap();
        core.poll_deferred_spatial_play();
        assert!(calls.actions.lock().unwrap().is_empty());
        slot.lock().unwrap().resume = SpatialResume::Ready;
        core.poll_deferred_spatial_play();
        core.poll_deferred_spatial_play();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Play
            }]
        ));
        assert_eq!(core.snapshot().state, PlaybackState::Paused);
        assert!(!slot.lock().unwrap().lease.as_ref().unwrap().playing);
        core.send(PlayerCommand::Pause).unwrap();
        assert_eq!(slot.lock().unwrap().resume, SpatialResume::Unavailable);
    }

    #[test]
    fn pause_and_device_changes_cancel_prepared_resume_before_dispatch() {
        let (mut core, slot, calls) = setup_paused_resume();
        core.send(PlayerCommand::Play).unwrap();
        core.send(PlayerCommand::Pause).unwrap();
        core.poll_deferred_spatial_play();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Pause
            }]
        ));
        assert_eq!(slot.lock().unwrap().resume, SpatialResume::Unavailable);

        slot.lock().unwrap().resume = SpatialResume::Warming;
        core.send(PlayerCommand::Play).unwrap();
        core.remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap()
            .device
            .as_mut()
            .unwrap()
            .id = "external".into();
        core.poll_deferred_spatial_play();
        assert_eq!(calls.actions.lock().unwrap().len(), 1);
        assert_eq!(slot.lock().unwrap().resume, SpatialResume::Unavailable);
    }

    #[test]
    fn warm_deadline_releases_old_lease_and_dispatches_direct_play() {
        let (mut core, slot, calls) = setup_paused_resume();
        core.send(PlayerCommand::Play).unwrap();
        core.host_analysis.deferred_play.as_mut().unwrap().deadline = Instant::now();
        core.poll_deferred_spatial_play();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [RemoteAction::Control {
                control: RemoteControl::Play
            }]
        ));
        assert!(slot.lock().unwrap().lease.is_none());
        assert_eq!(core.host_analysis_revoked_through(), Some(1));
    }

    #[test]
    fn queue_edit_preserves_deferred_play_only_for_the_same_owned_output() {
        let (mut core, slot, calls) = setup_paused_resume();
        let playback = core
            .remote
            .states
            .get_mut("fixture")
            .unwrap()
            .playback
            .as_mut()
            .unwrap();
        playback.transport.queue_add = true;
        let item = playback.current.clone().unwrap();
        core.send(PlayerCommand::Play).unwrap();
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Control {
                control: RemoteControl::Enqueue { item },
            },
        })
        .unwrap();
        assert!(slot.lock().unwrap().lease.is_none());
        core.poll_deferred_spatial_play();
        assert!(matches!(
            calls.actions.lock().unwrap().as_slice(),
            [
                RemoteAction::Control {
                    control: RemoteControl::Enqueue { .. }
                },
                RemoteAction::Control {
                    control: RemoteControl::Play
                }
            ]
        ));
    }
}
