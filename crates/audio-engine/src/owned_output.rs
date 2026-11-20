//! macOS process-only capture. PID comes from the host supervisor, independently
//! verified before tapping. No global tap, microphone, audio files or Swift PCM.
use audio_analysis::Analyzer;
use player_domain::{
    AnalysisFrame, HostAnalysisLease, HostAnalysisStatus as Issue, HostOutputAnalysis,
    HostSpatialStatus, SpatialResume,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
mod capture;
mod identity;
mod live_trace;
mod relay;

const RENDERER_STANDBY: Duration = Duration::from_secs(2);
const ROUTE_POLL: Duration = Duration::from_millis(100);
const PREWARM_IDLE: Duration = Duration::from_secs(4);
const PAUSED_SILENCE_AFTER: Duration = Duration::from_millis(600);
const PAUSED_SILENCE_LIMIT: Duration = Duration::from_secs(45);
const PAUSED_CAPTURE_LIMIT: Duration = Duration::from_secs(90);
const DORMANT_PAUSE_CONFIRM_LIMIT: Duration = Duration::from_millis(350);
// An idle AirPods route took 1.42 s to start before its usual negative preroll.
const RESUME_PREWARM_LIMIT: Duration = Duration::from_millis(1700);
const STANDBY_REQUEST_LIMIT: Duration = Duration::from_millis(1250);
const PREWARM_HOLD: Duration = Duration::from_secs(4);

#[derive(Clone, Copy)]
struct Prewarm {
    generation: u64,
    requested: Instant,
    ready: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PausedRendererPolicy {
    Cancel,
    Warm,
    Suspend,
}

struct PausedSilence {
    generation: u64,
    ready: bool,
    cancelled: bool,
    renderer_only: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StandbyTransition {
    Next,
    NaturalNext,
    Seek,
    Reanchor { replacement_generation: u64 },
}

impl StandbyTransition {
    fn trace_name(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::NaturalNext => "natural_next",
            Self::Seek => "seek",
            Self::Reanchor { .. } => "clock_reanchor",
        }
    }

    fn permits_running_preroll(self) -> bool {
        matches!(self, Self::Next | Self::NaturalNext | Self::Reanchor { .. })
    }
}

struct StandbyRequest {
    previous: HostAnalysisLease,
    deadline: Instant,
    transition: StandbyTransition,
}

impl StandbyRequest {
    fn is_current(&self, now: Instant) -> bool {
        now < self.deadline
    }

    fn accepts(&self, lease: &HostAnalysisLease) -> bool {
        let occurrence_matches = match self.transition {
            StandbyTransition::Next | StandbyTransition::NaturalNext => {
                lease.media_id != self.previous.media_id
            }
            StandbyTransition::Seek => lease.media_id == self.previous.media_id,
            StandbyTransition::Reanchor {
                replacement_generation,
            } => {
                lease.generation == replacement_generation
                    && lease.media_id == self.previous.media_id
            }
        };
        lease.playing
            && lease.spatial_stereo
            && lease.generation > self.previous.generation
            && occurrence_matches
            && lease.process_id == self.previous.process_id
            && lease.provider == self.previous.provider
            && lease.device_id == self.previous.device_id
    }

    fn permits_preroll(&self, lease: Option<&HostAnalysisLease>, now: Instant) -> bool {
        self.is_current(now)
            && lease.is_none_or(|lease| {
                self.transition.permits_running_preroll() && self.accepts(lease)
            })
    }

    fn permits_running_origin(&self, lease: &HostAnalysisLease) -> bool {
        self.transition.permits_running_preroll()
            && self.previous.playing
            && lease.playing
            && self.previous.same_output(lease)
    }
}

fn current_standby_preroll(state: &State, request: &Arc<StandbyRequest>, now: Instant) -> bool {
    state
        .standby_request
        .as_ref()
        .is_some_and(|armed| Arc::ptr_eq(armed, request))
        && request.permits_preroll(state.lease.as_ref(), now)
}

fn release_capture<C>(capture: C, generation: u64) {
    live_trace::event("capture_teardown_start", Some(generation));
    let began = Instant::now();
    drop(capture);
    live_trace::capture_teardown(generation, began.elapsed());
}

#[derive(Default)]
struct State {
    lease: Option<HostAnalysisLease>,
    status: Issue,
    spatial_status: HostSpatialStatus,
    published: Option<(AnalysisFrame, Instant)>,
    spatial_paused_since: Option<Instant>,
    paused_silence: Option<PausedSilence>,
    renderer_only_pause: Option<u64>,
    prewarm: Option<Prewarm>,
    standby_request: Option<Arc<StandbyRequest>>,
    paused_seek_invalidation: u64,
    paused_seek_generation: Option<u64>,
}

fn retained_seek_scope_matches(
    generation: Option<u64>,
    target: Option<&HostAnalysisLease>,
    desired: Option<&HostAnalysisLease>,
) -> bool {
    target.zip(desired).is_some_and(|(target, desired)| {
        generation == Some(target.generation) && target.same_output(desired)
    })
}
impl State {
    fn arm_held_capture_renderer(&mut self, scope: &PauseCaptureScope, now: Instant) -> bool {
        let Some(lease) = self.lease.as_ref() else {
            return false;
        };
        if !scope.can_start_renderer(lease, self.confirms_renderer_only_pause(lease), now) {
            return false;
        }
        // Publish Warming admission before AVFoundation open/enqueue can block.
        // A user Play then waits on the existing bounded resume mechanism.
        self.spatial_paused_since = Some(
            self.spatial_paused_since
                .map_or(scope.prepared_at, |old| old.min(scope.prepared_at)),
        );
        if let Some(silence) = &mut self.paused_silence {
            silence.ready = false;
            silence.renderer_only = true;
        }
        if let Some(request) = &mut self.prewarm
            && request.generation == scope.origin.generation
        {
            request.ready = false;
        }
        self.spatial_status = HostSpatialStatus::Paused;
        true
    }

    fn cancel_retained_seek_pause(
        &mut self,
        revision: u64,
        generation: Option<u64>,
        target: Option<&HostAnalysisLease>,
    ) -> bool {
        if self.paused_seek_invalidation != revision
            || !retained_seek_scope_matches(generation, target, self.lease.as_ref())
        {
            return false;
        }
        if let Some(silence) = &mut self.paused_silence
            && Some(silence.generation) == generation
        {
            silence.ready = false;
            silence.cancelled = true;
        }
        if let Some(request) = &mut self.prewarm
            && Some(request.generation) == generation
        {
            request.ready = false;
        }
        true
    }
    fn retains_expired_paused_silence(&self, now: Instant) -> bool {
        self.spatial_paused_since
            .is_some_and(|paused| now.saturating_duration_since(paused) >= PAUSED_SILENCE_LIMIT)
            && self
                .lease
                .as_ref()
                .is_some_and(|lease| self.confirms_renderer_only_pause(lease))
    }
    fn active_paused_silence(&self, now: Instant) -> Option<&PausedSilence> {
        let age = now.saturating_duration_since(self.spatial_paused_since?);
        self.paused_silence.as_ref().filter(|silence| {
            !silence.cancelled
                && (silence.renderer_only || age >= PAUSED_SILENCE_AFTER)
                && age < PAUSED_SILENCE_LIMIT
        })
    }

    fn confirms_renderer_only_pause(&self, lease: &HostAnalysisLease) -> bool {
        self.renderer_only_pause == Some(lease.generation)
            && self.lease.as_ref().is_some_and(|current| {
                current.same_output(lease) && current.spatial_stereo && !current.playing
            })
            && self
                .paused_silence
                .as_ref()
                .is_some_and(|silence| silence.generation == lease.generation && !silence.cancelled)
    }

    fn set_confirmed_spatial_pause(&mut self, generation: Option<u64>) -> bool {
        let generation = generation.filter(|&generation| {
            self.lease.as_ref().is_some_and(|lease| {
                lease.generation == generation && lease.spatial_stereo && !lease.playing
            })
        });
        if generation
            == self
                .paused_silence
                .as_ref()
                .map(|silence| silence.generation)
        {
            return false;
        }
        self.paused_silence = generation.map(|generation| PausedSilence {
            generation,
            ready: false,
            cancelled: false,
            renderer_only: false,
        });
        true
    }

    fn set_confirmed_renderer_only_pause(&mut self, generation: Option<u64>) -> bool {
        let generation = generation.filter(|&generation| {
            self.lease.as_ref().is_some_and(|lease| {
                lease.generation == generation && lease.spatial_stereo && !lease.playing
            })
        });
        if generation == self.renderer_only_pause {
            return false;
        }
        self.renderer_only_pause = generation;
        true
    }
}
pub struct OwnedOutputAnalysis {
    state: Arc<Mutex<State>>,
    enabled: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl OwnedOutputAnalysis {
    pub fn new() -> std::io::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let enabled = Arc::new(AtomicU64::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_state, worker_enabled, worker_playing, stopping) = (
            state.clone(),
            enabled.clone(),
            playing.clone(),
            stop.clone(),
        );
        let thread = thread::Builder::new()
            .name("auvra-owned-analysis".into())
            .spawn(move || run(&worker_state, &worker_enabled, &worker_playing, &stopping))?;
        Ok(Self {
            state,
            enabled,
            playing,
            stop,
            thread: Some(thread),
        })
    }
}
struct Session {
    // Rust drops fields in declaration order. Release Soloist's mute before
    // potentially blocking AVFoundation cleanup on a broken route.
    capture: capture::Capture,
    relay: Option<relay::Relay>,
    analyzer: Option<Analyzer>,
    last_sample: Option<u64>,
    last_identity_check: Instant,
    paused: bool,
    dropped_before_pause: u64,
}

enum SessionStart {
    Started,
    PausedBeforeIo,
}

struct PauseCaptureScope {
    origin: HostAnalysisLease,
    route: u32,
    prepared_at: Instant,
    confirmed: bool,
}

impl PauseCaptureScope {
    fn can_start_renderer(&self, lease: &HostAnalysisLease, confirmed: bool, now: Instant) -> bool {
        self.confirmed
            && confirmed
            && self.origin.same_output(lease)
            && !lease.playing
            && lease.spatial_stereo
            && now.saturating_duration_since(self.prepared_at) < PAUSED_SILENCE_LIMIT
    }

    fn allows(
        &mut self,
        lease: Option<&HostAnalysisLease>,
        route: Option<u32>,
        pause_confirmed: bool,
        now: Instant,
    ) -> bool {
        let Some(lease) = lease.filter(|lease| self.origin.same_output(lease)) else {
            return false;
        };
        let limit = if self.confirmed {
            PAUSED_CAPTURE_LIMIT
        } else {
            DORMANT_PAUSE_CONFIRM_LIMIT
        };
        if route != Some(self.route) || now.saturating_duration_since(self.prepared_at) >= limit {
            return false;
        }
        if lease.playing {
            return self.confirmed;
        }
        if pause_confirmed {
            self.confirmed = true;
            return true;
        }
        !self.confirmed
    }
}

struct PreparedCapture {
    capture: capture::Capture,
    scope: PauseCaptureScope,
    checked_at: Instant,
}

#[derive(Default)]
struct PreparedPlayback {
    standby: Option<Standby>,
    capture: Option<PreparedCapture>,
}

impl PreparedPlayback {
    fn hold_capture(&mut self, capture: capture::Capture, scope: PauseCaptureScope) {
        self.capture = capture.into_prepared_pause().map(|capture| {
            live_trace::event("capture_held_for_pause", None);
            PreparedCapture {
                capture,
                scope,
                checked_at: Instant::now(),
            }
        });
    }
}

struct Standby {
    renderer: relay::StandbyRenderer,
    identity: identity::Identity,
    origin: HostAnalysisLease,
    released_at: Instant,
    dormant_pause: Option<DormantPause>,
    dormant_checked_at: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DormantPause {
    AwaitingConfirmation,
    Confirmed,
}

impl DormantPause {
    fn limit(self) -> Duration {
        match self {
            Self::AwaitingConfirmation => DORMANT_PAUSE_CONFIRM_LIMIT,
            Self::Confirmed => PAUSED_SILENCE_LIMIT,
        }
    }
}

impl Standby {
    fn awaiting_pause(
        renderer: relay::StandbyRenderer,
        identity: identity::Identity,
        origin: &HostAnalysisLease,
        released_at: Instant,
    ) -> Self {
        Self {
            renderer,
            identity,
            origin: origin.clone(),
            released_at,
            dormant_pause: Some(DormantPause::AwaitingConfirmation),
            dormant_checked_at: None,
        }
    }

    fn matches(&self, lease: &HostAnalysisLease) -> bool {
        lease.spatial_stereo
            && lease.process_id == self.origin.process_id
            && lease.provider == self.origin.provider
            && lease.device_id == self.origin.device_id
    }

    fn is_unexpired(&self) -> bool {
        if self.dormant_pause == Some(DormantPause::Confirmed) && self.renderer.has_retained_pause()
        {
            return true;
        }
        self.released_at.elapsed()
            < self
                .dormant_pause
                .map_or(RENDERER_STANDBY, DormantPause::limit)
    }

    fn output_is_current(&self) -> bool {
        self.identity.alive() && crate::spatial::bluetooth_output_id() == Some(self.renderer.route)
    }

    fn is_current(&self) -> bool {
        self.is_unexpired() && self.output_is_current()
    }

    fn can_keep(&mut self, confirmed_same_output: bool) -> bool {
        if self.dormant_pause != Some(DormantPause::Confirmed) {
            return self.is_current();
        }
        if !confirmed_same_output {
            return false;
        }
        if self
            .dormant_checked_at
            .is_some_and(|checked| checked.elapsed() < ROUTE_POLL)
        {
            return true;
        }
        let current = self.output_is_current();
        self.dormant_checked_at = Some(Instant::now());
        current
    }

    fn keep_dormant_pause(
        &mut self,
        resuming: bool,
        authorized: impl Fn() -> bool,
    ) -> Result<bool, HostSpatialStatus> {
        if self.dormant_pause == Some(DormantPause::AwaitingConfirmation) {
            self.dormant_pause = Some(DormantPause::Confirmed);
            self.renderer
                .keep_paused_until(self.released_at + PAUSED_SILENCE_LIMIT);
        }
        if self.released_at.elapsed() >= PAUSED_SILENCE_LIMIT || self.renderer.has_retained_pause()
        {
            if !authorized() {
                return Err(HostSpatialStatus::DirectRendererInterrupted);
            }
            if resuming {
                let ready = self.renderer.resume_paused_silence(&authorized)?;
                if !authorized() {
                    return Err(HostSpatialStatus::DirectRendererInterrupted);
                }
                Ok(ready)
            } else {
                self.renderer.suspend_paused_silence(authorized)?;
                Ok(false)
            }
        } else {
            match self.renderer.keep_paused_warm() {
                Err(HostSpatialStatus::DirectOutputStalled)
                    if self.released_at.elapsed() >= PAUSED_SILENCE_LIMIT =>
                {
                    self.renderer.suspend_paused_silence(authorized)?;
                    Ok(false)
                }
                result => result,
            }
        }
    }
}

fn renderer_authorization(
    current: Option<&HostAnalysisLease>,
    opening: &HostAnalysisLease,
) -> relay::OpenAuthorization {
    use relay::OpenAuthorization;
    match current.filter(|lease| lease.same_output(opening)) {
        Some(lease) if lease.playing => OpenAuthorization::Playing,
        Some(lease) if lease.spatial_stereo => OpenAuthorization::PausedSameOutput,
        _ => OpenAuthorization::Revoked,
    }
}

fn capture_start_authorization(
    authorization: relay::OpenAuthorization,
    capture_enabled: bool,
) -> relay::OpenAuthorization {
    use relay::OpenAuthorization;
    match authorization {
        OpenAuthorization::Playing if !capture_enabled => OpenAuthorization::Revoked,
        _ => authorization,
    }
}

#[derive(Default)]
struct Observation {
    frame: Option<AnalysisFrame>,
    reset: bool,
    presenting: bool,
    resume_ready: bool,
}

struct SessionError {
    issue: Issue,
    spatial: HostSpatialStatus,
}

impl From<Issue> for SessionError {
    fn from(issue: Issue) -> Self {
        let spatial = match issue {
            Issue::UnverifiedOutput | Issue::ProcessUnavailable => HostSpatialStatus::DirectProcess,
            Issue::UnsupportedSystem | Issue::TapUnavailable => HostSpatialStatus::DirectTap,
            Issue::DeviceUnavailable => HostSpatialStatus::DirectTapDevice,
            Issue::UnsupportedFormat => HostSpatialStatus::DirectTapFormat,
            Issue::StartUnavailable => HostSpatialStatus::DirectTapStart,
            _ => HostSpatialStatus::Direct,
        };
        Self { issue, spatial }
    }
}

impl SessionError {
    fn relay(spatial: HostSpatialStatus) -> Self {
        Self {
            issue: if spatial == HostSpatialStatus::WaitingForRoute {
                Issue::DeviceUnavailable
            } else {
                Issue::StartUnavailable
            },
            spatial,
        }
    }

    fn trace_name(&self) -> &'static str {
        match self.spatial {
            HostSpatialStatus::WaitingForRoute => "route_lost",
            HostSpatialStatus::DirectRendererInterrupted => "fallback_renderer_interrupted",
            HostSpatialStatus::DirectOutputStalled => "fallback_output_stalled",
            HostSpatialStatus::DirectInputStalled => "fallback_input_stalled",
            HostSpatialStatus::DirectBuffer => "fallback_buffer",
            HostSpatialStatus::DirectRenderer => "fallback_renderer_start",
            _ => "fallback_capture",
        }
    }
}

#[derive(Default)]
struct RelayRoute {
    current: Option<u32>,
    checked: Option<Instant>,
    retried: Option<u32>,
}

impl RelayRoute {
    fn needs_check(&self, now: Instant) -> bool {
        self.checked
            .is_none_or(|checked| now - checked >= ROUTE_POLL)
    }

    fn observe(&mut self, observed: Option<u32>, now: Instant) -> bool {
        self.checked = Some(now);
        if observed != self.retried {
            self.retried = None;
        }
        std::mem::replace(&mut self.current, observed) != observed
    }

    fn failure(&mut self, status: HostSpatialStatus, now: Instant) -> Option<HostSpatialStatus> {
        if status != HostSpatialStatus::WaitingForRoute {
            return Some(status);
        }
        self.checked = Some(now);
        // If the route returns between polls, try it once after fresh stereo
        // validation. A persistent change or successful playback resets this.
        if self.current.is_some() && self.current != self.retried {
            self.retried = self.current.take();
            None
        } else {
            Some(status)
        }
    }
}

impl Session {
    fn prepare(
        lease: &HostAnalysisLease,
        enabled: &Arc<AtomicU64>,
        playing: &Arc<AtomicBool>,
        route: Option<u32>,
        prepared: &mut PreparedPlayback,
        authorization: impl Fn() -> relay::OpenAuthorization,
        warm_transition: impl Fn() -> Option<StandbyTransition>,
    ) -> Result<Option<Self>, SessionError> {
        objc2::rc::autoreleasepool(|_| {
            use relay::{OpenAuthorization, OpenResult};
            if live_trace::measure(
                "capture_lease_check_start",
                "capture_lease_check_done",
                &authorization,
            ) != OpenAuthorization::Playing
            {
                return Ok(None);
            }
            let mut early_preroll = None;
            let prime_renderer = |rate, identity: identity::Identity| {
                if let Some(route) = route.filter(|_| {
                    live_trace::measure(
                        "renderer_early_identity_alive_start",
                        "renderer_early_identity_alive_done",
                        || identity.alive(),
                    )
                }) {
                    early_preroll = relay::Relay::start_cold_preroll(rate, route, &authorization)
                        .ok()
                        .flatten();
                }
            };
            let prime_renderer =
                (route.is_some() && prepared.standby.is_none()).then_some(prime_renderer);
            let held_capture = prepared.capture.take().and_then(|mut held| {
                (held.capture.prepared_is_current()
                    && held.scope.allows(Some(lease), route, false, Instant::now()))
                .then_some(held)
            });
            let capture = if let Some(reused) = held_capture {
                match authorization() {
                    OpenAuthorization::Playing => {}
                    OpenAuthorization::PausedSameOutput => {
                        prepared.capture = Some(reused);
                        return Ok(None);
                    }
                    OpenAuthorization::Revoked => return Ok(None),
                }
                if let Some(mut prime_renderer) = prime_renderer {
                    prime_renderer(reused.capture.rate(), reused.capture.identity());
                }
                live_trace::event("capture_reused_after_pause", None);
                reused.capture
            } else {
                if authorization() != OpenAuthorization::Playing {
                    return Ok(None);
                }
                live_trace::event("capture_preparing", None);
                let capture = capture::Capture::prepare(
                    lease,
                    enabled.clone(),
                    playing.clone(),
                    route.is_some(),
                    prime_renderer,
                );
                let capture = match capture {
                    Ok(capture) => capture,
                    Err(issue) => {
                        prepared.standby = None;
                        return Err(issue.into());
                    }
                };
                live_trace::event("capture_prepared", None);
                capture
            };
            let at_renderer = authorization();
            if at_renderer == OpenAuthorization::Revoked {
                prepared.standby = None;
                live_trace::event("capture_lease_revoked_after_prepare", None);
                return Ok(None);
            }
            if at_renderer == OpenAuthorization::PausedSameOutput
                && let Some(route) = route
                && prepared.standby.as_ref().is_some_and(|old| {
                    old.origin.same_output(lease)
                        && old.dormant_pause == Some(DormantPause::Confirmed)
                })
            {
                prepared.hold_capture(
                    capture,
                    PauseCaptureScope {
                        origin: lease.clone(),
                        route,
                        prepared_at: Instant::now(),
                        confirmed: false,
                    },
                );
                return Ok(None);
            }
            let previous = prepared.standby.take();
            let relay = if let Some(route) = route {
                let mut natural_handoff = false;
                let mut reusable = previous.filter(|old| {
                    old.is_current()
                        && old.matches(lease)
                        && old.identity == capture.identity()
                        && (old.dormant_pause.is_none()
                            || old.origin.same_output(lease)
                            || warm_transition().is_some())
                });
                let mut requested_preroll_finished = false;
                if let Some(old) = &mut reusable
                    && warm_transition().is_some_and(StandbyTransition::permits_running_preroll)
                {
                    requested_preroll_finished = true;
                    old.renderer
                        .finish_requested_handoff(capture.rate(), route, old.released_at, || {
                            warm_transition().is_some()
                                && old.identity.alive()
                                && crate::spatial::bluetooth_output_id() == Some(route)
                        })
                        .map_err(SessionError::relay)?;
                }
                let reusable = reusable.map(|mut old| {
                    if old.dormant_pause == Some(DormantPause::Confirmed)
                        && old.origin.same_output(lease)
                    {
                        // A freshly playing original lease may use ready silence.
                        // Otherwise follow the normal launch without additional waiting.
                        if !old.renderer.paused_ready() {
                            old.renderer.cancel_prewarm();
                        }
                        return old.renderer;
                    }
                    let transition = warm_transition();
                    if transition == Some(StandbyTransition::NaturalNext) {
                        natural_handoff = true;
                        live_trace::event("renderer_natural_next_handoff", None);
                        return old.renderer;
                    }
                    let seek_released_at =
                        (transition == Some(StandbyTransition::Seek)).then_some(old.released_at);
                    if seek_released_at.is_some() {
                        live_trace::event("renderer_seek_handoff", None);
                    }
                    if !requested_preroll_finished {
                        old.renderer
                            .finish_warmup(capture.rate(), route, seek_released_at, || {
                                warm_transition().is_some()
                                    && old.identity.alive()
                                    && crate::spatial::bluetooth_output_id() == Some(route)
                            });
                    }
                    old.renderer
                });
                let natural_handoff_is_current = || {
                    warm_transition() == Some(StandbyTransition::NaturalNext)
                        && capture.identity().alive()
                };
                let running_handoff_is_current = || {
                    warm_transition().is_some_and(StandbyTransition::permits_running_preroll)
                        && capture.identity().alive()
                };
                match relay::Relay::open_for_session(
                    capture.rate(),
                    route,
                    reusable,
                    early_preroll,
                    natural_handoff.then_some(&natural_handoff_is_current as &dyn Fn() -> bool),
                    Some(&running_handoff_is_current),
                    &authorization,
                )
                .map_err(SessionError::relay)?
                {
                    OpenResult::Ready(relay) => Some(relay),
                    OpenResult::Dormant(renderer) => {
                        let released_at = Instant::now();
                        let identity = capture.identity();
                        prepared.hold_capture(
                            capture,
                            PauseCaptureScope {
                                origin: lease.clone(),
                                route,
                                prepared_at: released_at,
                                confirmed: false,
                            },
                        );
                        prepared.standby = Some(Standby::awaiting_pause(
                            renderer,
                            identity,
                            lease,
                            released_at,
                        ));
                        live_trace::event("renderer_cold_kept_pause_silence", None);
                        return Ok(None);
                    }
                    OpenResult::Cancelled => {
                        if authorization() == OpenAuthorization::PausedSameOutput {
                            prepared.hold_capture(
                                capture,
                                PauseCaptureScope {
                                    origin: lease.clone(),
                                    route,
                                    prepared_at: Instant::now(),
                                    confirmed: false,
                                },
                            );
                        }
                        return Ok(None);
                    }
                }
            } else {
                None
            };
            Ok(Some(Self {
                relay,
                capture,
                analyzer: None,
                last_sample: None,
                last_identity_check: Instant::now(),
                paused: false,
                dropped_before_pause: 0,
            }))
        })
    }

    fn start(
        &mut self,
        route: Option<u32>,
        generation: u64,
        enabled: &AtomicU64,
        playing: &AtomicBool,
        authorization: impl Fn() -> relay::OpenAuthorization,
    ) -> Result<SessionStart, SessionError> {
        use relay::OpenAuthorization;
        if !self.capture.process_is_alive() {
            return Err(Issue::ProcessUnavailable.into());
        }
        if route.is_some() && route != relay::stereo_bluetooth_output() {
            return Err(SessionError::relay(HostSpatialStatus::WaitingForRoute));
        }
        let start_authorization = || {
            capture_start_authorization(
                authorization(),
                enabled.load(Ordering::Acquire) == generation && playing.load(Ordering::Acquire),
            )
        };
        match start_authorization() {
            OpenAuthorization::Playing => {}
            OpenAuthorization::PausedSameOutput => return Ok(SessionStart::PausedBeforeIo),
            OpenAuthorization::Revoked => return Err(Issue::ProcessUnavailable.into()),
        }
        let mut route_lost = false;
        let mut paused_before_io = false;
        let started = self.capture.start(|| {
            if route.is_some() && route != crate::spatial::bluetooth_output_id() {
                route_lost = true;
                return Err(Issue::DeviceUnavailable);
            }
            match start_authorization() {
                OpenAuthorization::Playing => Ok(()),
                OpenAuthorization::PausedSameOutput => {
                    paused_before_io = true;
                    Err(Issue::ProcessUnavailable)
                }
                OpenAuthorization::Revoked => Err(Issue::ProcessUnavailable),
            }
        });
        // The closure can only reject before AudioDeviceStart. An attempted
        // hardware start, including one that fails, never takes this path.
        if paused_before_io {
            return Ok(SessionStart::PausedBeforeIo);
        }
        if enabled.load(Ordering::Acquire) != generation {
            return Err(Issue::ProcessUnavailable.into());
        }
        // HAL start can block while the system default changes underneath it.
        if route_lost
            || (started != Err(Issue::ProcessUnavailable)
                && route.is_some()
                && route != crate::spatial::bluetooth_output_id())
        {
            enabled.store(0, Ordering::Release);
            return Err(SessionError::relay(HostSpatialStatus::WaitingForRoute));
        }
        started?;
        live_trace::event("capture_started", None);
        if let Some(relay) = &mut self.relay {
            relay.capture_started();
        }
        Ok(SessionStart::Started)
    }

    fn into_pause_before_io(
        self,
        lease: &HostAnalysisLease,
        route: Option<u32>,
        prepared: &mut PreparedPlayback,
        authorization: impl Fn() -> relay::OpenAuthorization,
    ) -> Option<Standby> {
        let route = route?;
        let released_at = Instant::now();
        let identity = self.capture.identity();
        if authorization() != relay::OpenAuthorization::PausedSameOutput || !identity.alive() {
            return None;
        }
        prepared.hold_capture(
            self.capture,
            PauseCaptureScope {
                origin: lease.clone(),
                route,
                prepared_at: released_at,
                confirmed: false,
            },
        );
        let renderer = self.relay?.into_prepared_pause()?;
        (authorization() == relay::OpenAuthorization::PausedSameOutput
            && identity.alive()
            && released_at.elapsed() < DORMANT_PAUSE_CONFIRM_LIMIT)
            .then(|| Standby::awaiting_pause(renderer, identity, lease, released_at))
    }

    fn silence(&mut self) {
        if let Some(relay) = &mut self.relay {
            relay.silence();
        }
    }

    fn release_renderer(
        self,
        previous: &HostAnalysisLease,
        keep_paused_until: Option<Instant>,
        running_deadline: Option<Instant>,
        running_is_current: impl Fn() -> bool,
    ) -> Option<Standby> {
        let mut relay = self.relay?;
        if !self.paused {
            relay.silence();
        }
        let identity = self.capture.identity();
        // The outgoing tap and buffered PCM are never used for the new lease.
        // Give Soloist back direct output before flushing AVFoundation.
        release_capture(self.capture, previous.generation);
        let renderer = match running_deadline {
            Some(deadline) if !self.paused => relay
                .into_running_next_standby(deadline, || running_is_current() && identity.alive()),
            _ => relay.into_standby(keep_paused_until.filter(|_| self.paused)),
        }?;
        Some(Standby {
            renderer,
            identity,
            origin: previous.clone(),
            released_at: Instant::now(),
            dormant_pause: None,
            dormant_checked_at: None,
        })
    }

    fn step(
        &mut self,
        lease: &HostAnalysisLease,
        enabled: &AtomicU64,
        playing: &AtomicBool,
        prewarm: Option<Prewarm>,
        pause_policy: PausedRendererPolicy,
        suspend_authorized: impl Fn() -> bool,
    ) -> Result<Observation, SessionError> {
        if enabled.load(Ordering::Acquire) != lease.generation {
            return Ok(Observation::default());
        }
        let playing_now = || {
            lease.playing
                && enabled.load(Ordering::Acquire) == lease.generation
                && playing.load(Ordering::Acquire)
        };
        if !playing_now() {
            if !self.paused {
                if let Some(relay) = &mut self.relay {
                    relay.pause();
                }
                self.analyzer = None;
                self.last_sample = None;
                self.paused = true;
            }
            self.capture.discard_pending();
            self.dropped_before_pause = self.capture.dropped();
        }
        if self.last_identity_check.elapsed() >= Duration::from_millis(250) {
            self.last_identity_check = Instant::now();
            if !self.capture.process_is_alive() {
                return Err(Issue::ProcessUnavailable.into());
            }
        }
        if !playing_now() {
            let mut observation = Observation::default();
            if let Some(relay) = &mut self.relay {
                if let Some(prewarm) = prewarm
                    && !prewarm.ready
                    && prewarm.requested.elapsed() >= RESUME_PREWARM_LIMIT
                {
                    return Err(SessionError::relay(HostSpatialStatus::DirectOutputStalled));
                }
                if prewarm.is_some() || pause_policy == PausedRendererPolicy::Warm {
                    observation.resume_ready =
                        relay.prewarm_paused().map_err(SessionError::relay)?;
                } else if pause_policy == PausedRendererPolicy::Suspend {
                    relay
                        .suspend_paused_silence(suspend_authorized)
                        .map_err(SessionError::relay)?;
                } else {
                    relay.cancel_prewarm();
                }
            }
            return Ok(observation);
        }
        if self.paused {
            self.paused = false;
            if let Some(relay) = &mut self.relay {
                relay.capture_started();
            }
        }
        if lease.visuals != self.analyzer.is_some() {
            self.last_sample = None;
            self.analyzer = if lease.visuals {
                Some(Analyzer::new(self.capture.rate()).ok_or(Issue::UnsupportedFormat)?)
            } else {
                None
            };
        }
        let mut observation = Observation::default();
        let mut analyze = |sample: capture::Sample| {
            let Some(analyzer) = self.analyzer.as_mut() else {
                return;
            };
            if self
                .last_sample
                .is_some_and(|last| last.checked_add(1) != Some(sample.position))
                || sample
                    .samples
                    .iter()
                    .any(|value| !value.is_finite() || value.abs() > 1.0)
            {
                analyzer.reset();
                observation.frame = None;
                observation.reset = true;
            }
            self.last_sample = Some(sample.position);
            if let Some(frame) = analyzer.push(sample.samples, sample.position, lease.generation) {
                observation.frame = Some(frame);
            }
        };
        for _ in 0..8192 {
            if !playing_now() {
                return Ok(Observation::default());
            }
            if self.relay.as_ref().is_some_and(|relay| !relay.can_accept()) {
                break;
            }
            let Some(sample) = self.capture.pop() else {
                break;
            };
            if let Some(relay) = &mut self.relay {
                relay.push(sample).map_err(SessionError::relay)?;
            } else {
                analyze(sample);
            }
        }
        observation.presenting = if let Some(relay) = &mut self.relay {
            if self.capture.dropped() != self.dropped_before_pause {
                return Err(SessionError::relay(HostSpatialStatus::DirectBuffer));
            }
            relay
                .tick(analyze, playing_now)
                .map_err(SessionError::relay)?
        } else {
            false
        };
        // Direct analysis is tied to the capture clock. A relay feeds analysis
        // only for frames that reached the renderer's presentation clock.
        if let Some(frame) = &mut observation.frame {
            frame.dropped_frames = self
                .capture
                .dropped()
                .saturating_sub(self.dropped_before_pause);
            if self.relay.is_none()
                && self
                    .capture
                    .position()
                    .saturating_sub(frame.window_end_frame)
                    > u64::from(self.capture.rate()) / 5
            {
                observation.frame = None;
                observation.reset = true;
            }
        }
        Ok(observation)
    }
}

fn run(
    state: &Mutex<State>,
    enabled: &Arc<AtomicU64>,
    playing: &Arc<AtomicBool>,
    stop: &AtomicBool,
) {
    let mut target: Option<HostAnalysisLease> = None;
    let mut session: Option<Session> = None;
    let mut prepared = PreparedPlayback::default();
    let mut route = RelayRoute::default();
    let mut relay_failure = None;
    let mut attempted_visuals = false;
    let mut attempted_pause_capture = false;
    let mut attempted_pause_renderer = false;
    let mut identity_lost = false;
    let mut applied_paused_seek_invalidation = 0;

    while !stop.load(Ordering::Acquire) {
        let (
            desired,
            mut prewarm,
            standby_request,
            mut keep_paused_warm,
            mut confirms_dormant_pause,
            mut retain_paused_renderer,
            paused_seek_invalidation,
            paused_seek_generation,
        ) = {
            let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if current.prewarm.is_some_and(|request| {
                request.requested.elapsed() >= PREWARM_HOLD
                    || current
                        .lease
                        .as_ref()
                        .is_none_or(|lease| lease.generation != request.generation || lease.playing)
            }) {
                current.prewarm = None;
            }
            if current
                .standby_request
                .as_ref()
                .is_some_and(|request| !request.is_current(now))
            {
                current.standby_request = None;
            }
            (
                current.lease.clone(),
                current.prewarm,
                current.standby_request.clone(),
                current.active_paused_silence(now).is_some(),
                current
                    .lease
                    .as_ref()
                    .is_some_and(|lease| current.confirms_renderer_only_pause(lease)),
                current.retains_expired_paused_silence(now),
                current.paused_seek_invalidation,
                current.paused_seek_generation,
            )
        };
        if paused_seek_invalidation != applied_paused_seek_invalidation {
            // Consume even an inactive latch, so an old seek cannot invalidate
            // a later pause. Only the new retained clock is retired.
            applied_paused_seek_invalidation = paused_seek_invalidation;
            let mut retired = false;
            if retained_seek_scope_matches(
                paused_seek_generation,
                target.as_ref(),
                desired.as_ref(),
            ) {
                if let Some(open) = &mut session
                    && open
                        .relay
                        .as_mut()
                        .is_some_and(|relay| relay.invalidate_retained_pause())
                {
                    open.capture.discard_pending();
                    open.dropped_before_pause = open.capture.dropped();
                    open.last_sample = None;
                    open.analyzer = None;
                    retired = true;
                }
                if let Some(old) = &mut prepared.standby {
                    retired |= old.renderer.invalidate_retained_pause();
                }
            }
            if retired {
                keep_paused_warm = false;
                confirms_dormant_pause = false;
                retain_paused_renderer = false;
                let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
                if current.cancel_retained_seek_pause(
                    paused_seek_invalidation,
                    paused_seek_generation,
                    target.as_ref(),
                ) {
                    prewarm = current.prewarm;
                }
            }
        }
        let same_target = match (&target, &desired) {
            (Some(old), Some(new)) => old.same_output(new),
            (None, None) => true,
            _ => false,
        };
        let mut released_renderer = false;
        if !same_target {
            prepared.capture = None;
            if let (Some(previous), Some(open)) = (&target, session.take()) {
                let keep_paused_until = standby_request.as_ref().and_then(|request| {
                    (!request.previous.playing && request.previous.same_output(previous))
                        .then_some(request.deadline)
                });
                let running_request = standby_request
                    .as_ref()
                    .filter(|request| request.permits_running_origin(previous));
                prepared.standby = open.release_renderer(
                    previous,
                    keep_paused_until,
                    running_request.map(|request| request.deadline),
                    || {
                        running_request.is_some_and(|request| {
                            let current = state.lock().unwrap_or_else(|e| e.into_inner());
                            !stop.load(Ordering::Acquire)
                                && current_standby_preroll(&current, request, Instant::now())
                        })
                    },
                );
                released_renderer = prepared.standby.is_some();
                if prepared.standby.is_some()
                    && standby_request.as_ref().is_some_and(|request| {
                        request.transition == StandbyTransition::NaturalNext
                            && request.previous.same_output(previous)
                    })
                {
                    live_trace::event("renderer_natural_next_armed", None);
                }
            }
            route = RelayRoute::default();
            relay_failure = None;
            attempted_visuals = false;
            attempted_pause_capture = false;
            attempted_pause_renderer = false;
            identity_lost = false;
        } else if let (Some(old), Some(new)) = (&target, &desired) {
            if !new.visuals {
                attempted_visuals = false;
            }
            if !old.playing && new.playing && relay_failure.is_some() && !identity_lost {
                // A user resume can retry a failed renderer. A successfully
                // paused relay keeps its open tap and renderer instead.
                session = None;
                relay_failure = None;
                attempted_visuals = false;
            }
        }
        if prepared.standby.as_mut().is_some_and(|standby| {
            let confirmed = desired.as_ref().is_some_and(|lease| {
                standby.origin.same_output(lease) && (lease.playing || confirms_dormant_pause)
            }) || standby_request.as_ref().is_some_and(|request| {
                request.is_current(Instant::now())
                    && request.previous.same_output(&standby.origin)
                    && standby.is_unexpired()
                    && !standby.renderer.has_retained_pause()
            });
            !standby.can_keep(confirmed)
                || desired.as_ref().is_some_and(|new| !standby.matches(new))
        }) {
            prepared.standby = None;
        }
        let standby_request = standby_request.filter(|request| {
            request.is_current(Instant::now())
                && prepared
                    .standby
                    .as_ref()
                    .is_some_and(|old| old.origin.same_output(&request.previous))
        });
        let warm_transition = |lease: Option<&HostAnalysisLease>| {
            let request = standby_request.as_ref()?;
            let current = state.lock().unwrap_or_else(|e| e.into_inner());
            (!stop.load(Ordering::Acquire)
                && request.is_current(Instant::now())
                && current
                    .standby_request
                    .as_ref()
                    .is_some_and(|armed| Arc::ptr_eq(armed, request))
                && match (lease, current.lease.as_ref()) {
                    (None, None) => true,
                    (Some(lease), Some(desired)) => {
                        request.accepts(lease) && desired.same_output(lease) && desired.playing
                    }
                    _ => false,
                })
            .then_some(request.transition)
        };
        let warm_preroll_is_current = || {
            let request = standby_request.as_ref()?;
            let current = state.lock().unwrap_or_else(|e| e.into_inner());
            (!stop.load(Ordering::Acquire)
                && current_standby_preroll(&current, request, Instant::now()))
            .then_some(request.transition)
        };
        if released_renderer && let Some(old) = &prepared.standby {
            let current = state.lock().unwrap_or_else(|e| e.into_inner());
            let successor = current.lease.as_ref().is_some_and(|lease| {
                standby_request
                    .as_ref()
                    .is_some_and(|request| request.accepts(lease))
            });
            let armed = standby_request
                .as_ref()
                .is_some_and(|request| current_standby_preroll(&current, request, Instant::now()));
            drop(current);
            live_trace::handoff_scope(
                "handoff_after_teardown",
                old.origin.generation,
                successor,
                old.renderer.has_preroll(),
                armed,
            );
        }
        let mut dormant_paused = false;
        let mut waiting_dormant_resume = false;
        let mut drop_standby = false;
        if let Some(old) = &mut prepared.standby {
            let dormant_lease = desired
                .as_ref()
                .filter(|lease| old.dormant_pause.is_some() && old.origin.same_output(lease));
            if let Some(lease) = dormant_lease {
                let pause_is_current = || {
                    let current = state.lock().unwrap_or_else(|e| e.into_inner());
                    !stop.load(Ordering::Acquire)
                        && current.paused_seek_invalidation == paused_seek_invalidation
                        && current.lease.as_ref().is_some_and(|active| {
                            active.same_output(lease)
                                && if lease.playing {
                                    active.playing
                                } else {
                                    current.confirms_renderer_only_pause(active)
                                }
                        })
                };
                if lease.playing {
                    drop_standby = old.dormant_pause != Some(DormantPause::Confirmed);
                    if !drop_standby && old.renderer.has_retained_pause() {
                        match old.keep_dormant_pause(true, pause_is_current) {
                            Ok(ready) => waiting_dormant_resume = !ready,
                            Err(_) => drop_standby = true,
                        }
                    }
                } else if confirms_dormant_pause {
                    if let Ok(ready) = old.keep_dormant_pause(prewarm.is_some(), pause_is_current) {
                        dormant_paused = true;
                        let mut became_ready = false;
                        let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
                        if current.confirms_renderer_only_pause(lease) {
                            current.spatial_paused_since = Some(old.released_at);
                            if let Some(silence) = &mut current.paused_silence {
                                became_ready = ready && !silence.ready;
                                silence.ready = ready;
                                silence.renderer_only = true;
                            }
                            if ready
                                && let Some(prewarm) = &mut current.prewarm
                                && prewarm.generation == lease.generation
                            {
                                prewarm.ready = true;
                            }
                        }
                        drop(current);
                        if became_ready {
                            live_trace::event("renderer_dormant_pause_ready", None);
                        }
                    } else {
                        drop_standby = true;
                    }
                } else if old.dormant_pause == Some(DormantPause::Confirmed) {
                    drop_standby = true;
                }
            } else if old.dormant_pause.is_some() && standby_request.is_none() {
                drop_standby = true;
            } else if standby_request.is_none() {
                old.renderer.cancel_prewarm();
            } else if warm_preroll_is_current().is_some() && old.renderer.warm_next().is_err() {
                old.renderer.cancel_prewarm();
                let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
                if current
                    .standby_request
                    .as_ref()
                    .zip(standby_request.as_ref())
                    .is_some_and(|(armed, request)| Arc::ptr_eq(armed, request))
                {
                    current.standby_request = None;
                }
                drop(current);
                live_trace::event("renderer_next_warm_abandoned", None);
            }
        }
        if drop_standby {
            prepared.standby = None;
        }
        target = desired;
        let Some(lease) = &target else {
            thread::park_timeout(Duration::from_millis(if standby_request.is_some() {
                4
            } else {
                250
            }));
            continue;
        };
        let update = |apply: &mut dyn FnMut(&mut State)| {
            let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
            if current.lease.as_ref() == Some(lease) {
                apply(&mut current);
            }
        };

        let now = Instant::now();
        if lease.spatial_stereo && route.needs_check(now) {
            let relaying =
                dormant_paused || session.as_ref().is_some_and(|open| open.relay.is_some());
            if relaying && crate::spatial::bluetooth_output_id() != route.current {
                enabled.store(0, Ordering::Release);
                session = None;
                prepared.standby = None;
                dormant_paused = false;
                relay_failure = route.failure(HostSpatialStatus::WaitingForRoute, Instant::now());
                live_trace::event("route_lost", None);
                update(&mut |current| {
                    current.published = None;
                    current.prewarm = None;
                    current.paused_silence = None;
                    current.status = Issue::DeviceUnavailable;
                });
            } else {
                let current_route = if relaying {
                    route.current
                } else {
                    live_trace::event("relay_route_probe_start", None);
                    let observed = relay::stereo_bluetooth_output();
                    live_trace::event("relay_route_probe_done", None);
                    observed
                };
                if route.observe(current_route, now) {
                    session = None;
                    relay_failure = None;
                    attempted_visuals = false;
                    update(&mut |current| {
                        current.published = None;
                        current.paused_silence = None;
                    });
                }
            }
        }
        let relay_route = route
            .current
            .filter(|_| lease.spatial_stereo && relay_failure.is_none() && !identity_lost);
        attempted_pause_capture |= prepared.capture.is_some();
        if prepared.capture.as_mut().is_some_and(|held| {
            let scope_allowed = held
                .scope
                .allows(Some(lease), relay_route, confirms_dormant_pause, now);
            let process_alive = if scope_allowed
                && held.checked_at.elapsed() >= Duration::from_millis(250)
            {
                held.checked_at = now;
                Some(held.capture.process_is_alive())
            } else {
                None
            };
            let rejected = !scope_allowed || process_alive == Some(false);
            if rejected && live_trace::enabled() {
                let (current_same_output, current_playing, current_pause_confirmed) = {
                    let current = state.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        current.lease.as_ref().is_some_and(|current| held.scope.origin.same_output(current)),
                        current.lease.as_ref().is_some_and(|current| current.playing),
                        current.lease.as_ref().is_some_and(|lease| current.confirms_renderer_only_pause(lease)),
                    )
                };
                eprintln!(
                    "auvra_capture_hold scope_allowed={} scope_same={} route_same={} age_ms={} confirmed={} snapshot_playing={} snapshot_pause_confirmed={} process_alive={:?} current_same={} current_playing={} current_pause_confirmed={}",
                    scope_allowed,
                    held.scope.origin.same_output(lease),
                    relay_route == Some(held.scope.route),
                    now.saturating_duration_since(held.scope.prepared_at).as_millis(),
                    held.scope.confirmed,
                    lease.playing,
                    confirms_dormant_pause,
                    process_alive,
                    current_same_output,
                    current_playing,
                    current_pause_confirmed,
                );
            }
            rejected
        }) {
            live_trace::event("capture_pause_hold_release_begin", None);
            prepared.capture = None;
            live_trace::event("capture_pause_hold_ended", None);
        }
        if session.as_ref().is_some_and(|open| open.relay.is_none())
            && (!lease.visuals || !lease.playing)
        {
            session = None;
        }

        let authorization = || {
            let current = state.lock().unwrap_or_else(|e| e.into_inner());
            if stop.load(Ordering::Acquire) {
                relay::OpenAuthorization::Revoked
            } else {
                renderer_authorization(current.lease.as_ref(), lease)
            }
        };
        if session.is_none()
            && prepared.capture.is_none()
            && !lease.playing
            && confirms_dormant_pause
            && !attempted_pause_capture
            && let Some(route) = relay_route
        {
            let pause_is_current = || {
                !stop.load(Ordering::Acquire)
                    && state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .confirms_renderer_only_pause(lease)
            };
            if crate::spatial::bluetooth_output_id() == Some(route) && pause_is_current() {
                attempted_pause_capture = true;
                live_trace::event("capture_pause_prepare_started", None);
                let capture = objc2::rc::autoreleasepool(|_| {
                    capture::Capture::prepare(
                        lease,
                        enabled.clone(),
                        playing.clone(),
                        true,
                        None::<fn(u32, identity::Identity)>,
                    )
                });
                if let Ok(capture) = capture
                    && now.elapsed() < PAUSED_CAPTURE_LIMIT
                    && capture.process_is_alive()
                    && crate::spatial::bluetooth_output_id() == Some(route)
                    && (authorization() == relay::OpenAuthorization::Playing || pause_is_current())
                {
                    prepared.hold_capture(
                        capture,
                        PauseCaptureScope {
                            origin: lease.clone(),
                            route,
                            prepared_at: now,
                            confirmed: true,
                        },
                    );
                } else {
                    live_trace::event("capture_pause_prepare_abandoned", None);
                }
            }
        }

        if session.is_none()
            && prepared.standby.is_none()
            && !lease.playing
            && !attempted_pause_renderer
            && let Some(held) = prepared.capture.as_ref()
            && relay_route == Some(held.scope.route)
        {
            let armed = {
                let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
                current.paused_seek_invalidation == paused_seek_invalidation
                    && current.arm_held_capture_renderer(&held.scope, Instant::now())
            };
            if armed {
                attempted_pause_renderer = true;
                let identity = held.capture.identity();
                let original_pause = held.scope.prepared_at;
                let deadline = original_pause + PAUSED_SILENCE_LIMIT;
                let paused_authorization = || {
                    let scoped = {
                        let current = state.lock().unwrap_or_else(|e| e.into_inner());
                        !stop.load(Ordering::Acquire)
                            && current.paused_seek_invalidation == paused_seek_invalidation
                            && current.lease.as_ref().is_some_and(|current_lease| {
                                held.scope.can_start_renderer(
                                    current_lease,
                                    current.confirms_renderer_only_pause(current_lease),
                                    Instant::now(),
                                )
                            })
                    };
                    if scoped && identity.alive() {
                        relay::OpenAuthorization::PausedSameOutput
                    } else {
                        relay::OpenAuthorization::Revoked
                    }
                };
                live_trace::event("renderer_confirmed_pause_prepare_started", None);
                let renderer = if held.capture.prepared_is_current() {
                    relay::Relay::start_paused_preroll(
                        held.capture.rate(),
                        held.scope.route,
                        deadline,
                        paused_authorization,
                    )
                } else {
                    Ok(None)
                };
                if let Ok(Some(renderer)) = renderer
                    && paused_authorization() == relay::OpenAuthorization::PausedSameOutput
                {
                    prepared.standby = Some(Standby {
                        renderer,
                        identity,
                        origin: lease.clone(),
                        released_at: original_pause,
                        dormant_pause: Some(DormantPause::Confirmed),
                        dormant_checked_at: None,
                    });
                    dormant_paused = true;
                    live_trace::event("renderer_confirmed_pause_prepared", None);
                } else {
                    live_trace::event("renderer_confirmed_pause_prepare_abandoned", None);
                }
            }
        }

        if session.is_none()
            && !waiting_dormant_resume
            && lease.playing
            && !identity_lost
            && (relay_route.is_some() || lease.visuals && !attempted_visuals)
        {
            if relay_route.is_none() {
                attempted_visuals = true;
            }
            let is_current = || authorization() == relay::OpenAuthorization::Playing;
            let opened = Session::prepare(
                lease,
                enabled,
                playing,
                relay_route,
                &mut prepared,
                authorization,
                || warm_transition(Some(lease)),
            )
            .and_then(|open| {
                use relay::OpenAuthorization;
                let Some(mut open) = open else {
                    return Ok(None);
                };
                let current = state.lock().unwrap_or_else(|e| e.into_inner());
                if stop.load(Ordering::Acquire) {
                    return Ok(None);
                }
                let at_launch = renderer_authorization(current.lease.as_ref(), lease);
                if at_launch == OpenAuthorization::Playing {
                    enabled.store(lease.generation, Ordering::Release);
                }
                drop(current);
                let outcome = match at_launch {
                    OpenAuthorization::Playing => open.start(
                        relay_route,
                        lease.generation,
                        enabled,
                        playing,
                        authorization,
                    )?,
                    OpenAuthorization::PausedSameOutput => SessionStart::PausedBeforeIo,
                    OpenAuthorization::Revoked => return Ok(None),
                };
                match outcome {
                    SessionStart::Started => Ok(Some(open)),
                    SessionStart::PausedBeforeIo => {
                        enabled.store(0, Ordering::Release);
                        prepared.standby = open.into_pause_before_io(
                            lease,
                            relay_route,
                            &mut prepared,
                            authorization,
                        );
                        if prepared.standby.is_some() {
                            live_trace::event("renderer_ready_kept_pause_silence", None);
                        }
                        Ok(None)
                    }
                }
            });
            match opened {
                Ok(Some(open)) if is_current() => {
                    session = Some(open);
                    update(&mut |current| current.status = Issue::WaitingForAudio);
                }
                Err(error) if is_current() => {
                    enabled.store(0, Ordering::Release);
                    live_trace::event(error.trace_name(), None);
                    if relay_route.is_some() {
                        relay_failure = route.failure(error.spatial, Instant::now());
                    }
                    update(&mut |current| {
                        if error.spatial == HostSpatialStatus::WaitingForRoute {
                            current.published = None;
                            current.prewarm = None;
                        }
                        current.status = error.issue;
                    });
                }
                _ => {
                    enabled.store(0, Ordering::Release);
                    live_trace::event("capture_start_cancelled", None);
                }
            }
        }

        let mut presenting = false;
        if let Some(open) = &mut session {
            let suspend_authorized = || {
                if stop.load(Ordering::Acquire)
                    || enabled.load(Ordering::Acquire) != lease.generation
                    || playing.load(Ordering::Acquire)
                {
                    return false;
                }
                let current = state.lock().unwrap_or_else(|e| e.into_inner());
                current.paused_seek_invalidation == paused_seek_invalidation
                    && current
                        .lease
                        .as_ref()
                        .is_some_and(|active| active.same_output(lease))
                    && current.retains_expired_paused_silence(Instant::now())
            };
            match open.step(
                lease,
                enabled,
                playing,
                prewarm,
                if keep_paused_warm {
                    PausedRendererPolicy::Warm
                } else if retain_paused_renderer {
                    PausedRendererPolicy::Suspend
                } else {
                    PausedRendererPolicy::Cancel
                },
                suspend_authorized,
            ) {
                Ok(observation) => {
                    presenting = observation.presenting;
                    if presenting {
                        route.retried = None;
                    }
                    let mut became_ready = false;
                    let mut kept_warm = false;
                    update(&mut |current| {
                        if keep_paused_warm
                            && let Some(silence) = &mut current.paused_silence
                            && silence.generation == lease.generation
                            && !silence.cancelled
                        {
                            kept_warm = observation.resume_ready && !silence.ready;
                            silence.ready = observation.resume_ready;
                        }
                        if observation.resume_ready
                            && let Some(request) = &mut current.prewarm
                            && prewarm.is_some_and(|began| began.requested == request.requested)
                            && !request.ready
                        {
                            request.ready = true;
                            became_ready = true;
                        }
                        if let Some(frame) = observation.frame {
                            current.published = Some((frame, Instant::now()));
                            current.status = Issue::Running;
                        } else if observation.reset {
                            current.published = None;
                            current.status = Issue::WaitingForAudio;
                        }
                    });
                    if became_ready {
                        live_trace::event("renderer_prewarm_ready", None);
                    }
                    if kept_warm {
                        live_trace::event("renderer_pause_silence_ready", None);
                    }
                }
                Err(error) => {
                    let relaying = open.relay.is_some();
                    identity_lost = error.issue == Issue::ProcessUnavailable;
                    enabled.store(0, Ordering::Release);
                    session = None;
                    live_trace::event(error.trace_name(), None);
                    if relaying {
                        relay_failure = route.failure(error.spatial, Instant::now());
                    }
                    update(&mut |current| {
                        current.published = None;
                        current.paused_silence = None;
                        if error.spatial == HostSpatialStatus::WaitingForRoute {
                            current.prewarm = None;
                        }
                        current.status = error.issue;
                    });
                }
            }
        }
        if lease.spatial_stereo {
            update(&mut |current| {
                if !lease.playing
                    && (dormant_paused || session.as_ref().is_some_and(|open| open.relay.is_some()))
                {
                    current
                        .spatial_paused_since
                        .get_or_insert_with(Instant::now);
                } else {
                    current.spatial_paused_since = None;
                    current.prewarm = None;
                    if let Some(silence) = &mut current.paused_silence
                        && silence.renderer_only
                    {
                        silence.ready = false;
                        silence.renderer_only = false;
                    }
                }
                current.spatial_status = if presenting {
                    HostSpatialStatus::Active
                } else if let Some(reason) = relay_failure {
                    reason
                } else if identity_lost {
                    HostSpatialStatus::DirectProcess
                } else if !lease.playing && (dormant_paused || session.is_some()) {
                    HostSpatialStatus::Paused
                } else if route.current.is_none() {
                    HostSpatialStatus::WaitingForRoute
                } else {
                    HostSpatialStatus::Starting
                };
            });
        }
        thread::park_timeout(Duration::from_millis(if waiting_dormant_resume {
            4
        } else if dormant_paused || keep_paused_warm && prewarm.is_none() {
            12
        } else if !lease.playing && prewarm.is_none() {
            25
        } else if session.is_some() {
            4
        } else {
            100
        }));
    }
    enabled.store(0, Ordering::Release);
    if let Some(open) = &mut session {
        open.silence();
    }
}

impl OwnedOutputAnalysis {
    fn set_lease_locked(&self, state: &mut State, lease: Option<HostAnalysisLease>) {
        if state
            .standby_request
            .as_ref()
            .is_some_and(|request| lease.as_ref().is_none_or(|lease| !request.accepts(lease)))
        {
            state.standby_request = None;
            self.wake();
        }
        self.replace_lease(state, lease);
    }

    fn replace_lease(&self, state: &mut State, lease: Option<HostAnalysisLease>) {
        if state.lease == lease {
            return;
        }
        live_trace::event(
            match lease.as_ref() {
                Some(lease) if lease.playing => "lease_playing",
                Some(_) => "lease_paused",
                None => "lease_revoked",
            },
            lease.as_ref().map(|lease| lease.generation),
        );
        let same_output = state
            .lease
            .as_ref()
            .zip(lease.as_ref())
            .is_some_and(|(old, new)| old.same_output(new));
        if !same_output {
            self.enabled.store(0, Ordering::Release);
            state.spatial_paused_since = None;
            state.prewarm = None;
            state.spatial_status = if lease.as_ref().is_some_and(|lease| lease.spatial_stereo) {
                HostSpatialStatus::Starting
            } else {
                HostSpatialStatus::Idle
            };
        }
        if !same_output || lease.as_ref().is_some_and(|lease| lease.playing) {
            state.paused_silence = None;
            state.renderer_only_pause = None;
        }
        self.playing.store(
            lease.as_ref().is_some_and(|lease| lease.playing),
            Ordering::Release,
        );
        state.status = if lease
            .as_ref()
            .is_some_and(|lease| lease.visuals && lease.playing)
        {
            Issue::Starting
        } else {
            Issue::Idle
        };
        state.lease = lease;
        state.published = None;
        self.wake();
    }

    fn revoke_for_spatial_transition(
        &mut self,
        generation: u64,
        transition: StandbyTransition,
        absolute_deadline: Option<Instant>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let lease = state
            .lease
            .as_ref()
            .filter(|lease| lease.generation == generation && lease.spatial_stereo);
        if matches!(transition, StandbyTransition::Reanchor { .. }) && lease.is_none() {
            // A delayed observation cannot revoke a different current lease.
            return;
        }
        let capture_enabled = self.enabled.load(Ordering::Acquire) == generation;
        let status = state.spatial_status;
        let was_playing = lease.is_some_and(|lease| lease.playing);
        state.standby_request = lease.and_then(|lease| {
            let limit = now + STANDBY_REQUEST_LIMIT;
            let deadline = absolute_deadline.map_or(limit, |deadline| deadline.min(limit));
            if deadline <= now
                || matches!(
                    transition,
                    StandbyTransition::Reanchor { replacement_generation }
                        if replacement_generation <= lease.generation
                )
            {
                return None;
            }
            let deadline = match (transition, lease.playing, state.spatial_status) {
                (_, true, HostSpatialStatus::Active) if capture_enabled => deadline,
                (StandbyTransition::Next, false, HostSpatialStatus::Paused)
                    if state.active_paused_silence(now).is_some_and(|silence| {
                        silence.generation == generation
                            && silence.ready
                            && (capture_enabled
                                || silence.renderer_only
                                    && state.confirms_renderer_only_pause(lease))
                    }) =>
                {
                    deadline.min(state.spatial_paused_since? + PAUSED_SILENCE_LIMIT)
                }
                _ => return None,
            };
            Some(Arc::new(StandbyRequest {
                previous: lease.clone(),
                deadline,
                transition,
            }))
        });
        let armed = state.standby_request.is_some();
        self.replace_lease(&mut state, None);
        drop(state);
        live_trace::handoff_admission(
            generation,
            transition.trace_name(),
            status,
            capture_enabled,
            was_playing,
            armed,
        );
        self.wake();
    }

    fn wake(&self) {
        if let Some(worker) = &self.thread {
            worker.thread().unpark();
        }
    }
}

impl HostOutputAnalysis for OwnedOutputAnalysis {
    fn set_lease(&mut self, lease: Option<HostAnalysisLease>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.set_lease_locked(&mut state, lease);
    }

    fn set_lease_with_pause_proofs(
        &mut self,
        lease: Option<HostAnalysisLease>,
        confirmed_pause: Option<u64>,
        renderer_only_pause: Option<u64>,
    ) {
        // Waking the worker while holding this lock cannot expose a new paused
        // lease before the evidence which preserves its prepared capture.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.set_lease_locked(&mut state, lease);
        let pause_changed = state.set_confirmed_spatial_pause(confirmed_pause);
        let renderer_changed = state.set_confirmed_renderer_only_pause(renderer_only_pause);
        if pause_changed || renderer_changed {
            self.wake();
        }
    }

    fn revoke_for_spatial_next(&mut self, generation: u64) {
        self.revoke_for_spatial_transition(generation, StandbyTransition::Next, None);
    }

    fn revoke_for_natural_spatial_next(&mut self, generation: u64) {
        self.revoke_for_spatial_transition(generation, StandbyTransition::NaturalNext, None);
    }

    fn revoke_for_spatial_seek(&mut self, generation: u64) {
        self.revoke_for_spatial_transition(generation, StandbyTransition::Seek, None);
    }

    fn revoke_for_spatial_reanchor(
        &mut self,
        old_generation: u64,
        replacement_generation: u64,
        deadline: Instant,
    ) {
        self.revoke_for_spatial_transition(
            old_generation,
            StandbyTransition::Reanchor {
                replacement_generation,
            },
            Some(deadline),
        );
    }

    fn cancel_spatial_next(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.standby_request.take().is_some() {
            self.wake();
        }
    }
    fn status(&self) -> Issue {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).status
    }
    fn spatial_status(&self) -> HostSpatialStatus {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spatial_status
    }
    fn set_confirmed_spatial_pause(&mut self, generation: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.set_confirmed_spatial_pause(generation) {
            self.wake();
        }
    }
    fn set_confirmed_renderer_only_pause(&mut self, generation: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.set_confirmed_renderer_only_pause(generation) {
            self.wake();
        }
    }
    fn prepare_spatial_resume(&mut self, generation: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let warmed = state
            .active_paused_silence(now)
            .map(|silence| silence.ready);
        if !state.lease.as_ref().is_some_and(|lease| {
            lease.generation == generation && lease.spatial_stereo && !lease.playing
        }) || warmed.is_none()
            && state
                .spatial_paused_since
                .is_none_or(|paused| now.saturating_duration_since(paused) < PREWARM_IDLE)
        {
            return false;
        }
        state.prewarm = Some(Prewarm {
            generation,
            requested: now,
            ready: warmed.unwrap_or(false),
        });
        if let Some(worker) = &self.thread {
            worker.thread().unpark();
        }
        true
    }
    fn spatial_resume(&self, generation: u64) -> SpatialResume {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prewarm
            .filter(|request| request.generation == generation)
            .map_or(SpatialResume::Unavailable, |request| {
                if request.ready {
                    SpatialResume::Ready
                } else {
                    SpatialResume::Warming
                }
            })
    }
    fn cancel_spatial_resume(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.prewarm.take().is_some() {
            if let Some(silence) = &mut state.paused_silence {
                silence.cancelled = true;
                silence.ready = false;
            }
            self.wake();
        }
    }
    fn invalidate_spatial_resume_after_seek(&mut self, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.lease.as_ref().is_some_and(|lease| {
            lease.generation == generation && lease.spatial_stereo && !lease.playing
        }) {
            // Latched until the worker consumes it. Do not cancel the ordinary
            // zero-origin warm path or touch the already paused process tap.
            state.paused_seek_invalidation = state.paused_seek_invalidation.wrapping_add(1);
            state.paused_seek_generation = Some(generation);
            self.wake();
        }
    }
    fn analysis(&self) -> Option<AnalysisFrame> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .published
            .filter(|(_, time)| time.elapsed() <= Duration::from_millis(200))
            .filter(|(frame, _)| self.enabled.load(Ordering::Acquire) == frame.generation)
            .map(|(frame, _)| frame)
    }
}
impl Drop for OwnedOutputAnalysis {
    fn drop(&mut self) {
        self.enabled.store(0, Ordering::Release);
        self.playing.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use player_domain::MediaId;

    fn detached_output() -> OwnedOutputAnalysis {
        OwnedOutputAnalysis {
            state: Arc::default(),
            enabled: Arc::default(),
            playing: Arc::default(),
            stop: Arc::default(),
            thread: None,
        }
    }

    #[test]
    fn next_successor_arriving_during_capture_drop_still_authorizes_zero_preroll() {
        use std::sync::mpsc;
        struct CaptureDrop {
            entered: mpsc::SyncSender<()>,
            finish: mpsc::Receiver<()>,
        }
        impl Drop for CaptureDrop {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                self.finish.recv_timeout(Duration::from_secs(2)).unwrap();
            }
        }
        for transition in [StandbyTransition::Next, StandbyTransition::NaturalNext] {
            let mut output = detached_output();
            let origin = spatial_lease(true);
            output.set_lease(Some(origin.clone()));
            output.enabled.store(origin.generation, Ordering::Release);
            output.state.lock().unwrap().spatial_status = HostSpatialStatus::Active;
            output.revoke_for_spatial_transition(origin.generation, transition, None);
            let request = output
                .state
                .lock()
                .unwrap()
                .standby_request
                .clone()
                .unwrap();
            let state = output.state.clone();
            let (entered, inside_drop) = mpsc::sync_channel(0);
            let (finish, release_drop) = mpsc::sync_channel(0);
            let finished = Arc::new(AtomicBool::new(false));
            let worker_finished = finished.clone();
            let worker = thread::spawn(move || {
                release_capture(
                    CaptureDrop {
                        entered,
                        finish: release_drop,
                    },
                    origin.generation,
                );
                worker_finished.store(true, Ordering::Release);
                let state = state.lock().unwrap();
                assert!(
                    state.lease.is_some(),
                    "The incoming lease must precede teardown completion"
                );
                current_standby_preroll(&state, &request, Instant::now())
            });
            inside_drop.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(!finished.load(Ordering::Acquire));
            let mut successor = spatial_lease(true);
            successor.generation += 1;
            successor.media_id = MediaId::parse("spotify:track:2222222222222222222222").unwrap();
            output.set_lease(Some(successor));
            finish.send(()).unwrap();
            assert!(
                worker.join().unwrap(),
                "A verified successor must not lose preroll admission because capture Drop returned late"
            );
        }
    }

    #[test]
    fn coalesced_preroll_never_uses_a_foreign_paused_expired_or_replaced_request() {
        let now = Instant::now();
        let origin = spatial_lease(true);
        let request = Arc::new(StandbyRequest {
            previous: origin.clone(),
            deadline: now + STANDBY_REQUEST_LIMIT,
            transition: StandbyTransition::Next,
        });
        for fault in [
            "process",
            "provider",
            "device",
            "old media",
            "old generation",
            "paused",
            "nonspatial",
            "expired",
            "replaced",
            "revoked",
            "seek",
        ] {
            let mut successor = origin.clone();
            successor.generation += 1;
            successor.media_id = MediaId::parse("spotify:track:2222222222222222222222").unwrap();
            match fault {
                "process" => successor.process_id += 1,
                "provider" => successor.provider = "other".into(),
                "device" => successor.device_id = "other".into(),
                "old media" => successor.media_id = origin.media_id.clone(),
                "old generation" => successor.generation = origin.generation,
                "paused" => successor.playing = false,
                "nonspatial" => successor.spatial_stereo = false,
                _ => {}
            }
            let mut state = State {
                lease: Some(successor),
                standby_request: Some(request.clone()),
                ..Default::default()
            };
            let mut active = request.clone();
            if fault == "replaced" {
                state.standby_request = Some(Arc::new(StandbyRequest {
                    previous: origin.clone(),
                    deadline: request.deadline,
                    transition: StandbyTransition::Next,
                }));
            } else if fault == "revoked" {
                state.standby_request = None;
            } else if fault == "seek" {
                active = Arc::new(StandbyRequest {
                    previous: origin.clone(),
                    deadline: request.deadline,
                    transition: StandbyTransition::Seek,
                });
                state.lease.as_mut().unwrap().media_id = origin.media_id.clone();
                state.standby_request = Some(active.clone());
            }
            let at = if fault == "expired" {
                request.deadline
            } else {
                now
            };
            assert!(!current_standby_preroll(&state, &active, at), "{fault}");
        }
        let state = State {
            standby_request: Some(request.clone()),
            ..Default::default()
        };
        assert!(current_standby_preroll(&state, &request, now));
        assert!(!current_standby_preroll(&state, &request, request.deadline));
    }

    fn spatial_lease(playing: bool) -> HostAnalysisLease {
        HostAnalysisLease {
            generation: 4,
            process_id: 10,
            provider: "original".into(),
            device_id: "owned-output".into(),
            media_id: MediaId::parse("original:track:previous").unwrap(),
            spatial_stereo: true,
            visuals: false,
            playing,
        }
    }

    #[test]
    fn cancelled_cold_open_can_hold_only_a_pause_from_the_opening_lease() {
        use relay::OpenAuthorization;
        let opening = spatial_lease(true);
        let mut current = opening.clone();
        assert!(matches!(
            renderer_authorization(Some(&current), &opening),
            OpenAuthorization::Playing
        ));
        current.playing = false;
        assert!(matches!(
            renderer_authorization(Some(&current), &opening),
            OpenAuthorization::PausedSameOutput
        ));
        assert!(matches!(
            renderer_authorization(None, &opening),
            OpenAuthorization::Revoked
        ));
        for invalid in [
            HostAnalysisLease {
                generation: current.generation + 1,
                ..current.clone()
            },
            HostAnalysisLease {
                process_id: current.process_id + 1,
                ..current.clone()
            },
            HostAnalysisLease {
                device_id: "another-output".into(),
                ..current.clone()
            },
            HostAnalysisLease {
                media_id: MediaId::parse("original:track:next").unwrap(),
                ..current.clone()
            },
            HostAnalysisLease {
                spatial_stereo: false,
                ..current
            },
        ] {
            assert!(matches!(
                renderer_authorization(Some(&invalid), &opening),
                OpenAuthorization::Revoked
            ));
        }
    }

    #[test]
    fn a_disabled_capture_can_only_hold_an_explicit_same_output_pause() {
        use relay::OpenAuthorization;
        assert!(matches!(
            capture_start_authorization(OpenAuthorization::Playing, false),
            OpenAuthorization::Revoked
        ));
        assert!(matches!(
            capture_start_authorization(OpenAuthorization::Playing, true),
            OpenAuthorization::Playing
        ));
        for capture_enabled in [false, true] {
            assert!(matches!(
                capture_start_authorization(OpenAuthorization::PausedSameOutput, capture_enabled),
                OpenAuthorization::PausedSameOutput
            ));
            assert!(matches!(
                capture_start_authorization(OpenAuthorization::Revoked, capture_enabled),
                OpenAuthorization::Revoked
            ));
        }
    }

    #[test]
    fn renderer_only_next_requires_explicit_confirmation_and_ready_silence_without_capture() {
        let mut output = detached_output();
        let paused = spatial_lease(false);
        let arm = |output: &mut OwnedOutputAnalysis, confirmed: Option<u64>, ready| {
            output.set_lease(Some(paused.clone()));
            output.set_confirmed_spatial_pause(Some(paused.generation));
            output.set_confirmed_renderer_only_pause(confirmed);
            let paused_at = Instant::now() - PAUSED_SILENCE_LIMIT + Duration::from_secs(1);
            {
                let mut state = output.state.lock().unwrap();
                state.spatial_status = HostSpatialStatus::Paused;
                state.spatial_paused_since = Some(paused_at);
                let silence = state.paused_silence.as_mut().unwrap();
                silence.ready = ready;
                silence.renderer_only = true;
            }
            assert_eq!(output.enabled.load(Ordering::Acquire), 0);
            output.revoke_for_spatial_next(paused.generation);
            assert_eq!(output.enabled.load(Ordering::Acquire), 0);
            paused_at + PAUSED_SILENCE_LIMIT
        };

        for (confirmed, ready) in [
            (None, true),
            (Some(paused.generation + 1), true),
            (Some(paused.generation), false),
        ] {
            arm(&mut output, confirmed, ready);
            assert!(output.state.lock().unwrap().standby_request.is_none());
        }
        let expires = arm(&mut output, Some(paused.generation), true);
        let state = output.state.lock().unwrap();
        let request = state.standby_request.as_ref().unwrap();
        assert_eq!(request.deadline, expires);
        assert!(state.renderer_only_pause.is_none());
        assert!(state.lease.is_none());
    }

    #[test]
    fn transient_route_loss_allows_one_fresh_retry_even_when_the_same_device_returns() {
        let now = Instant::now();
        let mut route = RelayRoute::default();
        assert!(route.observe(Some(7), now));
        let lost = HostSpatialStatus::WaitingForRoute;
        assert_eq!(route.failure(lost, now), None);
        assert_eq!(route.current, None);
        assert!(!route.needs_check(now + ROUTE_POLL - Duration::from_millis(1)));
        let next = now + ROUTE_POLL;
        assert!(route.needs_check(next));
        assert!(route.observe(Some(7), next));
        assert_eq!(route.failure(lost, next), Some(lost));
        assert!(!route.observe(Some(7), next + ROUTE_POLL));
        assert!(route.observe(None, next + 2 * ROUTE_POLL));
        assert!(route.observe(Some(7), next + 3 * ROUTE_POLL));
        assert_eq!(route.failure(lost, next + 3 * ROUTE_POLL), None);
    }

    #[test]
    fn capture_and_renderer_failures_stay_latched_on_an_unchanged_route() {
        let now = Instant::now();
        let mut route = RelayRoute::default();
        assert!(route.observe(Some(7), now));
        for failure in [
            SessionError::from(Issue::DeviceUnavailable).spatial,
            HostSpatialStatus::DirectRendererInterrupted,
            HostSpatialStatus::DirectBuffer,
        ] {
            assert_eq!(route.failure(failure, now), Some(failure));
            assert_eq!(route.current, Some(7));
            assert!(!route.observe(Some(7), now + ROUTE_POLL));
        }
    }

    #[test]
    fn next_standby_revokes_capture_and_accepts_only_a_fresh_compatible_lease() {
        let mut output = detached_output();
        let previous = spatial_lease(true);
        let arm = |output: &mut OwnedOutputAnalysis| {
            output.set_lease(Some(previous.clone()));
            output.enabled.store(previous.generation, Ordering::Release);
            output.state.lock().unwrap().spatial_status = HostSpatialStatus::Active;
            output.revoke_for_spatial_next(previous.generation);
        };
        arm(&mut output);
        assert_eq!(output.enabled.load(Ordering::Acquire), 0);
        assert!(!output.playing.load(Ordering::Acquire));
        let token = output
            .state
            .lock()
            .unwrap()
            .standby_request
            .clone()
            .unwrap();
        assert!(output.state.lock().unwrap().lease.is_none());
        let mut next = previous.clone();
        assert!(!token.accepts(&next));
        next.media_id = MediaId::parse("original:track:next").unwrap();
        assert!(!token.accepts(&next));
        next.generation += 1;
        assert!(token.accepts(&next));
        next.process_id += 1;
        assert!(!token.accepts(&next));
        next.process_id = previous.process_id;
        next.playing = false;
        assert!(!token.accepts(&next));
        next.playing = true;
        output.cancel_spatial_next();
        assert!(output.state.lock().unwrap().standby_request.is_none());

        arm(&mut output);
        output.set_lease(Some(next));
        assert!(output.state.lock().unwrap().standby_request.is_some());
        output.set_lease(None);
        assert!(output.state.lock().unwrap().standby_request.is_none());
    }

    #[test]
    fn running_next_standby_requires_the_exact_playing_origin_and_a_live_token() {
        let now = Instant::now();
        let origin = spatial_lease(true);
        let mut paused = origin.clone();
        paused.playing = false;
        let mut another_generation = origin.clone();
        another_generation.generation += 1;
        for transition in [StandbyTransition::Next, StandbyTransition::NaturalNext] {
            let request = Arc::new(StandbyRequest {
                previous: origin.clone(),
                deadline: now + STANDBY_REQUEST_LIMIT,
                transition,
            });
            assert!(request.permits_running_origin(&origin));
            assert!(!request.permits_running_origin(&paused));
            assert!(!request.permits_running_origin(&another_generation));
            let mut state = State {
                standby_request: Some(request.clone()),
                ..State::default()
            };
            assert!(current_standby_preroll(&state, &request, now));
            assert!(!current_standby_preroll(&state, &request, request.deadline));
            state.lease = Some(paused.clone());
            assert!(!current_standby_preroll(&state, &request, now));
            state.lease = None;
            state.standby_request = None;
            assert!(!current_standby_preroll(&state, &request, now));
        }
        let seek = StandbyRequest {
            previous: origin.clone(),
            deadline: now + STANDBY_REQUEST_LIMIT,
            transition: StandbyTransition::Seek,
        };
        assert!(!seek.permits_running_origin(&origin));
    }

    #[test]
    fn clock_reanchor_can_preserve_only_the_exact_fresh_same_media_replacement() {
        let mut output = detached_output();
        let origin = spatial_lease(true);
        let mut replacement = origin.clone();
        replacement.generation += 2;
        let install_origin = |output: &mut OwnedOutputAnalysis| {
            output.set_lease(Some(origin.clone()));
            output.enabled.store(origin.generation, Ordering::Release);
            output.state.lock().unwrap().spatial_status = HostSpatialStatus::Active;
        };
        install_origin(&mut output);
        let now = Instant::now();
        let deadline = now + Duration::from_millis(500);
        output.revoke_for_spatial_reanchor(origin.generation, replacement.generation, deadline);
        assert_eq!(output.enabled.load(Ordering::Acquire), 0);
        assert!(!output.playing.load(Ordering::Acquire));
        let token = {
            let state = output.state.lock().unwrap();
            assert!(state.lease.is_none());
            state.standby_request.clone().unwrap()
        };
        assert_eq!(token.deadline, deadline);
        assert!(token.permits_running_origin(&origin));
        assert!(token.permits_preroll(Some(&replacement), now));
        assert!(!token.permits_preroll(Some(&replacement), deadline));
        for wrong in [
            HostAnalysisLease {
                generation: replacement.generation - 1,
                ..replacement.clone()
            },
            HostAnalysisLease {
                generation: replacement.generation + 1,
                ..replacement.clone()
            },
            HostAnalysisLease {
                media_id: MediaId::parse("original:track:next").unwrap(),
                ..replacement.clone()
            },
            HostAnalysisLease {
                device_id: "new-device".into(),
                ..replacement.clone()
            },
            HostAnalysisLease {
                process_id: replacement.process_id + 1,
                ..replacement.clone()
            },
            HostAnalysisLease {
                playing: false,
                ..replacement.clone()
            },
        ] {
            assert!(!token.permits_preroll(Some(&wrong), now));
        }
        output.set_lease(Some(replacement.clone()));
        output.revoke_for_spatial_reanchor(origin.generation, replacement.generation + 1, deadline);
        {
            let state = output.state.lock().unwrap();
            assert_eq!(state.lease.as_ref(), Some(&replacement));
            assert!(current_standby_preroll(&state, &token, now));
        }
        output.cancel_spatial_next();
        assert!(!current_standby_preroll(
            &output.state.lock().unwrap(),
            &token,
            now
        ));

        install_origin(&mut output);
        output.revoke_for_spatial_reanchor(origin.generation, replacement.generation, now);
        let state = output.state.lock().unwrap();
        assert!(state.lease.is_none());
        assert!(state.standby_request.is_none());
        assert_eq!(output.enabled.load(Ordering::Acquire), 0);
    }

    #[test]
    fn natural_next_standby_starts_only_from_active_capture() {
        let mut output = detached_output();
        let previous = spatial_lease(true);
        let paused = spatial_lease(false);
        output.set_lease(Some(paused.clone()));
        output.set_confirmed_spatial_pause(Some(paused.generation));
        output.enabled.store(paused.generation, Ordering::Release);
        {
            let mut state = output.state.lock().unwrap();
            state.spatial_status = HostSpatialStatus::Paused;
            state.spatial_paused_since = Some(Instant::now());
            state.paused_silence.as_mut().unwrap().ready = true;
        }
        output.revoke_for_natural_spatial_next(paused.generation);
        assert!(output.state.lock().unwrap().standby_request.is_none());

        for (status, enabled) in [
            (HostSpatialStatus::Starting, previous.generation),
            (HostSpatialStatus::Active, 0),
        ] {
            output.set_lease(Some(previous.clone()));
            output.enabled.store(enabled, Ordering::Release);
            output.state.lock().unwrap().spatial_status = status;
            output.revoke_for_natural_spatial_next(previous.generation);
            assert!(output.state.lock().unwrap().standby_request.is_none());
        }

        output.set_lease(Some(previous.clone()));
        output.enabled.store(previous.generation, Ordering::Release);
        output.state.lock().unwrap().spatial_status = HostSpatialStatus::Active;
        output.revoke_for_natural_spatial_next(previous.generation);
        assert_eq!(output.enabled.load(Ordering::Acquire), 0);
        assert!(!output.playing.load(Ordering::Acquire));
        let mut next = HostAnalysisLease {
            generation: previous.generation + 1,
            ..previous
        };
        {
            let state = output.state.lock().unwrap();
            let request = state.standby_request.as_ref().unwrap();
            assert!(request.transition == StandbyTransition::NaturalNext);
            assert!(!request.is_current(request.deadline));
            assert!(!request.accepts(&next));
            next.media_id = MediaId::parse("original:track:next").unwrap();
            assert!(request.accepts(&next));
        }
        output.set_lease(Some(next));
        assert!(output.state.lock().unwrap().standby_request.is_some());
    }

    #[test]
    fn seek_standby_requires_a_new_playing_lease_for_the_same_owned_media() {
        let mut output = detached_output();
        let previous = spatial_lease(true);
        let arm = |output: &mut OwnedOutputAnalysis| {
            output.set_lease(Some(previous.clone()));
            output.enabled.store(previous.generation, Ordering::Release);
            output.state.lock().unwrap().spatial_status = HostSpatialStatus::Active;
            output.revoke_for_spatial_seek(previous.generation);
            assert_eq!(output.enabled.load(Ordering::Acquire), 0);
            assert!(!output.playing.load(Ordering::Acquire));
            let state = output.state.lock().unwrap();
            assert!(state.lease.is_none());
            state.standby_request.clone().unwrap()
        };
        let token = arm(&mut output);
        assert!(!token.accepts(&previous));
        assert!(!token.is_current(token.deadline));
        let mut next = previous.clone();
        next.generation += 1;
        assert!(token.accepts(&next));
        assert!(!token.accepts(&HostAnalysisLease {
            process_id: next.process_id + 1,
            ..next.clone()
        }));
        assert!(!token.accepts(&HostAnalysisLease {
            device_id: "another-output".into(),
            ..next.clone()
        }));
        assert!(!token.accepts(&HostAnalysisLease {
            playing: false,
            ..next.clone()
        }));
        output.set_lease(Some(HostAnalysisLease {
            media_id: MediaId::parse("original:track:another").unwrap(),
            ..next.clone()
        }));
        assert!(output.state.lock().unwrap().standby_request.is_none());

        arm(&mut output);
        output.set_lease(Some(next));
        assert!(output.state.lock().unwrap().standby_request.is_some());
        output.cancel_spatial_next();
        assert!(output.state.lock().unwrap().standby_request.is_none());
    }

    #[test]
    fn paused_next_transfers_only_ready_confirmed_silence_before_its_original_deadline() {
        let mut output = detached_output();
        let previous = spatial_lease(false);
        let ready_pause = PAUSED_SILENCE_LIMIT - Duration::from_secs(1);
        let arm = |output: &mut OwnedOutputAnalysis, confirmed: Option<bool>, age, transition| {
            output.set_lease(Some(previous.clone()));
            output.enabled.store(previous.generation, Ordering::Release);
            if confirmed.is_some() {
                output.set_confirmed_spatial_pause(Some(previous.generation));
            }
            let paused_at = Instant::now() - age;
            {
                let mut state = output.state.lock().unwrap();
                state.spatial_status = HostSpatialStatus::Paused;
                state.spatial_paused_since = Some(paused_at);
                if let Some(silence) = &mut state.paused_silence {
                    silence.ready = confirmed.unwrap();
                }
            }
            output.revoke_for_spatial_transition(previous.generation, transition, None);
            assert_eq!(output.enabled.load(Ordering::Acquire), 0);
            paused_at + PAUSED_SILENCE_LIMIT
        };
        for (confirmed, age, transition) in [
            (None, ready_pause, StandbyTransition::Next),
            (Some(false), ready_pause, StandbyTransition::Next),
            (Some(true), PAUSED_SILENCE_LIMIT, StandbyTransition::Next),
            (Some(true), ready_pause, StandbyTransition::Seek),
        ] {
            arm(&mut output, confirmed, age, transition);
            assert!(output.state.lock().unwrap().standby_request.is_none());
        }
        let expires = arm(
            &mut output,
            Some(true),
            ready_pause,
            StandbyTransition::Next,
        );
        output.set_confirmed_spatial_pause(None);
        let token = output
            .state
            .lock()
            .unwrap()
            .standby_request
            .clone()
            .unwrap();
        assert!(!token.previous.playing);
        assert_eq!(token.deadline, expires);
        assert!(token.is_current(expires - Duration::from_millis(1)));
        assert!(!token.is_current(expires));
    }

    fn prepared_pause_capture(now: Instant) -> PauseCaptureScope {
        PauseCaptureScope {
            origin: spatial_lease(true),
            route: 7,
            prepared_at: now,
            confirmed: false,
        }
    }

    #[test]
    fn rapid_resume_then_pause_publishes_a_coherent_capture_scope() {
        let mut output = detached_output();
        let now = Instant::now();
        let paused = spatial_lease(false);
        let generation = Some(paused.generation);
        let mut capture = prepared_pause_capture(now);
        capture.confirmed = true;

        // A worker can run between the legacy setters. After a short Play,
        // it sees the next Pause without evidence and discards the held graph.
        output.set_lease_with_pause_proofs(Some(spatial_lease(true)), None, None);
        output.set_lease(Some(paused.clone()));
        {
            let state = output.state.lock().unwrap();
            assert!(!capture.allows(
                state.lease.as_ref(),
                Some(7),
                state.confirms_renderer_only_pause(&paused),
                now,
            ));
        }

        // Each worker-visible snapshot now includes the proof for that lease.
        // Repeated heartbeats must also keep the existing ready silence intact.
        for _ in 0..3 {
            output.set_lease_with_pause_proofs(Some(spatial_lease(true)), None, None);
            {
                let state = output.state.lock().unwrap();
                assert!(state.paused_silence.is_none());
                assert!(state.renderer_only_pause.is_none());
                assert!(capture.allows(state.lease.as_ref(), Some(7), false, now));
            }
            output.set_lease_with_pause_proofs(Some(paused.clone()), generation, generation);
            {
                let state = output.state.lock().unwrap();
                assert!(state.confirms_renderer_only_pause(&paused));
                assert!(capture.allows(state.lease.as_ref(), Some(7), true, now));
            }
            {
                let mut state = output.state.lock().unwrap();
                state.paused_silence.as_mut().unwrap().ready = true;
            }
            output.set_lease_with_pause_proofs(Some(paused.clone()), generation, generation);
            assert!(
                output
                    .state
                    .lock()
                    .unwrap()
                    .paused_silence
                    .as_ref()
                    .unwrap()
                    .ready
            );
        }
    }

    #[test]
    fn batched_pause_proofs_clear_with_missing_or_foreign_leases() {
        let mut output = detached_output();
        let paused = spatial_lease(false);
        let generation = Some(paused.generation);
        for (lease, pause, renderer_only) in [
            (None, generation, generation),
            (Some(paused.clone()), None, None),
            (
                Some(paused.clone()),
                Some(paused.generation + 1),
                Some(paused.generation + 1),
            ),
            (Some(spatial_lease(true)), generation, generation),
            (
                Some(HostAnalysisLease {
                    generation: paused.generation + 1,
                    ..paused.clone()
                }),
                generation,
                generation,
            ),
        ] {
            output.set_lease_with_pause_proofs(Some(paused.clone()), generation, generation);
            assert!(
                output
                    .state
                    .lock()
                    .unwrap()
                    .confirms_renderer_only_pause(&paused)
            );
            output.set_lease_with_pause_proofs(lease, pause, renderer_only);
            let state = output.state.lock().unwrap();
            assert!(state.paused_silence.is_none());
            assert!(state.renderer_only_pause.is_none());
            assert!(!state.confirms_renderer_only_pause(&paused));
        }
    }

    #[test]
    fn prepared_capture_only_accepts_resume_after_a_fresh_stationary_pause() {
        let now = Instant::now();
        let paused = spatial_lease(false);
        let playing = spatial_lease(true);
        let before_deadline = now + DORMANT_PAUSE_CONFIRM_LIMIT - Duration::from_millis(1);
        let mut pending = prepared_pause_capture(now);

        assert!(pending.allows(Some(&paused), Some(7), false, before_deadline));
        assert!(!pending.allows(Some(&playing), Some(7), true, before_deadline));
        for proof in [false, true] {
            let mut expired = prepared_pause_capture(now);
            assert!(!expired.allows(
                Some(&paused),
                Some(7),
                proof,
                now + DORMANT_PAUSE_CONFIRM_LIMIT,
            ));
        }

        let mut confirmed = prepared_pause_capture(now);
        assert!(confirmed.allows(Some(&paused), Some(7), true, before_deadline));
        let later = now + DORMANT_PAUSE_CONFIRM_LIMIT + Duration::from_secs(1);
        assert!(confirmed.allows(Some(&paused), Some(7), true, later));
        assert!(confirmed.allows(Some(&playing), Some(7), false, later));

        let mut lost_proof = prepared_pause_capture(now);
        assert!(lost_proof.allows(Some(&paused), Some(7), true, now));
        assert!(!lost_proof.allows(
            Some(&paused),
            Some(7),
            false,
            now + Duration::from_millis(1)
        ));
    }

    #[test]
    fn paused_prepared_capture_drops_on_lease_or_route_changes() {
        let now = Instant::now();
        let paused = spatial_lease(false);
        let mut confirmed = prepared_pause_capture(now);
        assert!(confirmed.allows(Some(&paused), Some(7), true, now));

        for lease in [
            HostAnalysisLease {
                generation: paused.generation + 1,
                ..paused.clone()
            },
            HostAnalysisLease {
                process_id: paused.process_id + 1,
                ..paused.clone()
            },
            HostAnalysisLease {
                provider: "other".into(),
                ..paused.clone()
            },
            HostAnalysisLease {
                device_id: "another-output".into(),
                ..paused.clone()
            },
            HostAnalysisLease {
                media_id: MediaId::parse("original:track:next").unwrap(),
                ..paused.clone()
            },
            HostAnalysisLease {
                spatial_stereo: false,
                ..paused.clone()
            },
        ] {
            assert!(!confirmed.allows(Some(&lease), Some(7), true, now));
        }
        assert!(!confirmed.allows(None, Some(7), true, now));
        assert!(!confirmed.allows(Some(&paused), None, true, now));
        assert!(!confirmed.allows(Some(&paused), Some(8), true, now));
    }

    #[test]
    fn confirmed_capture_expiry_does_not_extend_synthetic_silence() {
        let now = Instant::now();
        let paused = spatial_lease(false);
        let mut confirmed = prepared_pause_capture(now);
        assert!(confirmed.allows(Some(&paused), Some(7), true, now));
        let state = State {
            spatial_paused_since: Some(now),
            paused_silence: Some(PausedSilence {
                generation: paused.generation,
                ready: true,
                cancelled: false,
                renderer_only: true,
            }),
            ..State::default()
        };
        let silence_deadline = now + PAUSED_SILENCE_LIMIT;
        assert!(
            state
                .active_paused_silence(silence_deadline - Duration::from_millis(1))
                .is_some()
        );
        assert!(state.active_paused_silence(silence_deadline).is_none());
        assert!(confirmed.allows(Some(&paused), Some(7), true, silence_deadline));

        let capture_deadline = now + PAUSED_CAPTURE_LIMIT;
        assert!(confirmed.allows(
            Some(&paused),
            Some(7),
            true,
            capture_deadline - Duration::from_millis(1),
        ));
        assert!(!confirmed.allows(Some(&paused), Some(7), true, capture_deadline));
        assert!(!confirmed.allows(Some(&spatial_lease(true)), Some(7), false, capture_deadline));
    }

    #[test]
    fn held_capture_renderer_arms_bounded_resume_before_native_enqueue_without_enabling_io() {
        let mut output = detached_output();
        let paused = spatial_lease(false);
        output.set_lease_with_pause_proofs(
            Some(paused.clone()),
            Some(paused.generation),
            Some(paused.generation),
        );
        let now = Instant::now();
        let scope = PauseCaptureScope {
            origin: paused.clone(),
            route: 7,
            prepared_at: now,
            confirmed: true,
        };
        assert!(!output.prepare_spatial_resume(paused.generation));
        {
            let mut state = output.state.lock().unwrap();
            assert!(state.arm_held_capture_renderer(&scope, now));
            assert_eq!(state.spatial_paused_since, Some(now));
            assert_eq!(state.lease.as_ref(), Some(&paused));
            assert!(!state.paused_silence.as_ref().unwrap().ready);
        }
        // AVFoundation has not even been constructed yet. A competing Play
        // already sees Warming, so Soloist remains paused until Ready/timeout.
        assert!(output.prepare_spatial_resume(paused.generation));
        assert_eq!(
            output.spatial_resume(paused.generation),
            SpatialResume::Warming
        );
        assert_eq!(output.enabled.load(Ordering::Acquire), 0);
        assert!(!output.playing.load(Ordering::Acquire));
        let deadline = now + PAUSED_SILENCE_LIMIT;
        assert!(
            !output
                .state
                .lock()
                .unwrap()
                .arm_held_capture_renderer(&scope, deadline)
        );
        assert_eq!(output.state.lock().unwrap().spatial_paused_since, Some(now));
        let mut scope = scope;
        assert!(scope.allows(Some(&paused), Some(7), true, now + PAUSED_SILENCE_LIMIT));
        assert!(!scope.allows(Some(&paused), Some(7), true, now + PAUSED_CAPTURE_LIMIT));
    }

    #[test]
    fn held_capture_renderer_never_arms_for_restore_unconfirmed_or_changed_scope() {
        let now = Instant::now();
        for invalid in [
            "restore",
            "unconfirmed capture",
            "playing",
            "generation",
            "process",
            "provider",
            "device",
            "media",
            "cancelled",
            "expired",
        ] {
            let paused = spatial_lease(false);
            let mut state = State {
                lease: Some(paused.clone()),
                ..State::default()
            };
            let mut scope = PauseCaptureScope {
                origin: paused.clone(),
                route: 7,
                prepared_at: now,
                confirmed: true,
            };
            if invalid != "restore" {
                state.set_confirmed_spatial_pause(Some(paused.generation));
                state.set_confirmed_renderer_only_pause(Some(paused.generation));
            }
            let current = state.lease.as_mut().unwrap();
            match invalid {
                "unconfirmed capture" => scope.confirmed = false,
                "playing" => current.playing = true,
                "generation" => current.generation += 1,
                "process" => current.process_id += 1,
                "provider" => current.provider = "foreign".into(),
                "device" => current.device_id = "foreign".into(),
                "media" => current.media_id = MediaId::parse("original:track:other").unwrap(),
                "cancelled" => state.paused_silence.as_mut().unwrap().cancelled = true,
                _ => {}
            }
            let observed = if invalid == "expired" {
                now + PAUSED_SILENCE_LIMIT
            } else {
                now
            };
            assert!(
                !state.arm_held_capture_renderer(&scope, observed),
                "{invalid}"
            );
            assert!(state.spatial_paused_since.is_none(), "{invalid}");
            assert!(state.prewarm.is_none(), "{invalid}");
        }
    }

    #[test]
    fn stopped_renderer_retention_requires_expiry_and_uncancelled_confirmed_pause() {
        let now = Instant::now();
        let paused = spatial_lease(false);
        let mut state = State {
            lease: Some(paused.clone()),
            renderer_only_pause: Some(paused.generation),
            spatial_paused_since: Some(now),
            paused_silence: Some(PausedSilence {
                generation: paused.generation,
                ready: true,
                cancelled: false,
                renderer_only: true,
            }),
            ..State::default()
        };
        let expired = now + PAUSED_SILENCE_LIMIT;
        assert!(!state.retains_expired_paused_silence(expired - Duration::from_millis(1)));
        assert!(state.active_paused_silence(expired).is_none());
        assert!(state.retains_expired_paused_silence(expired));
        state.paused_silence.as_mut().unwrap().cancelled = true;
        assert!(!state.retains_expired_paused_silence(expired));
        state.paused_silence.as_mut().unwrap().cancelled = false;
        state.renderer_only_pause = None;
        assert!(!state.retains_expired_paused_silence(expired));
        state.renderer_only_pause = Some(paused.generation + 1);
        assert!(!state.retains_expired_paused_silence(expired));
        state.renderer_only_pause = Some(paused.generation);
        state.lease.as_mut().unwrap().playing = true;
        assert!(!state.retains_expired_paused_silence(expired));
    }

    #[test]
    fn seek_invalidation_is_latched_without_cancelling_the_ordinary_paused_warmup() {
        let mut output = detached_output();
        let paused = spatial_lease(false);
        output.set_lease(Some(paused.clone()));
        output.set_confirmed_spatial_pause(Some(paused.generation));
        output.set_confirmed_renderer_only_pause(Some(paused.generation));
        {
            let mut state = output.state.lock().unwrap();
            state.spatial_paused_since = Some(Instant::now() - Duration::from_secs(1));
            state.paused_silence.as_mut().unwrap().ready = true;
        }
        output.invalidate_spatial_resume_after_seek(paused.generation + 1);
        assert_eq!(output.state.lock().unwrap().paused_seek_invalidation, 0);
        output.invalidate_spatial_resume_after_seek(paused.generation);
        output.invalidate_spatial_resume_after_seek(paused.generation);
        {
            let state = output.state.lock().unwrap();
            assert_eq!(state.paused_seek_invalidation, 2);
            assert_eq!(state.lease.as_ref(), Some(&paused));
            assert!(
                state
                    .active_paused_silence(Instant::now())
                    .is_some_and(|silence| silence.ready && !silence.cancelled)
            );
        }
        output.set_lease(Some(spatial_lease(true)));
        output.invalidate_spatial_resume_after_seek(paused.generation);
        assert_eq!(output.state.lock().unwrap().paused_seek_invalidation, 2);
    }

    #[test]
    fn coalesced_old_seek_cannot_cancel_a_new_confirmed_pause() {
        let mut output = detached_output();
        let previous = spatial_lease(false);
        output.set_lease(Some(previous.clone()));
        output.invalidate_spatial_resume_after_seek(previous.generation);
        let current = HostAnalysisLease {
            generation: previous.generation + 1,
            ..previous.clone()
        };
        output.set_lease(Some(current.clone()));
        output.set_confirmed_spatial_pause(Some(current.generation));
        output.set_confirmed_renderer_only_pause(Some(current.generation));
        let mut state = output.state.lock().unwrap();
        state.paused_silence.as_mut().unwrap().ready = true;
        state.prewarm = Some(Prewarm {
            generation: current.generation,
            requested: Instant::now(),
            ready: true,
        });
        let revision = state.paused_seek_invalidation;
        let generation = state.paused_seek_generation;
        assert!(!retained_seek_scope_matches(
            generation,
            Some(&previous),
            Some(&current)
        ));
        assert!(!state.cancel_retained_seek_pause(revision, generation, Some(&previous)));
        assert!(
            state
                .paused_silence
                .as_ref()
                .is_some_and(|silence| silence.ready && !silence.cancelled)
        );
        assert!(state.prewarm.is_some_and(|request| request.ready));
        assert_eq!(state.lease.as_ref(), Some(&current));
    }
}
