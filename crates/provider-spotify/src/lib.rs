//! Spotify Web API / Connect. Transport IO is isolated from catalog/queue/discovery IO.
mod api;
mod autoplay;
mod autoplay_trace;
mod catalog;
mod cooldown;
mod mapping;
mod output_clock;
mod owned_commands;
mod owned_http;
#[cfg(test)]
mod tests;
mod transport_trace;
use api::Api;
use cooldown::Cooldown;
use player_domain::*;
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use transport_trace::{Reason as RouteReason, Route, Stage as TransportStage};

#[derive(Default)]
struct Mailbox {
    generation: u64,
    credential: Option<Option<String>>,
    auxiliary_token: Option<String>,
    authorization_revoked: bool,
    rate_limit: Option<Cooldown>,
    developer_quota: bool,
    control_backoff: Option<Cooldown>,
    control_backoff_issue: Option<PlayerError>,
    cancelled_control_result: Option<RemoteControlResult>,
    accepted_context_resume: Option<output_clock::ContextResumeReceipt>,
    resume_dispatch: Option<ResumeDispatch>,
    auxiliary_failure: Option<api::ApiFailure>,
    device_discovery: DeviceDiscovery,
    request_queue: Option<QueueRequest>,
    autoplay_request: Option<autoplay::Request>,
    autoplay_result: Option<(u64, Option<autoplay::Page>)>,
    trace_events: VecDeque<autoplay_trace::Event>,
    local_queue_binding: Option<QueueBinding>,
    devices: Option<Vec<PlaybackDevice>>,
    queue: Option<(QueueRequest, Vec<MediaItem>)>,
    controls: VecDeque<QueuedControl>,
    owned_http: Option<owned_http::Lane>,
    owned_http_serial: u64,
    owned_http_issue: bool,
    playback_intent_serial: u64,
    http_mutating: bool,
    http_starting_targeted_item: bool,
    direct_owned_start: Option<DirectOwnedStart>,
    query: Option<RemoteQuery>,
    latest_query_id: Option<u64>,
    context_page: Option<catalog::ContextPageGuard>,
    resume_album_tracks: VecDeque<api::AlbumTrackMembership>,
    details: VecDeque<RemoteQuery>,
    detail_credits: HashSet<u64>,
    detail_pages: VecDeque<RemotePage>,
    edit: Option<RemoteEditRequest>,
    edit_result: Option<RemoteEditResult>,
    edit_busy: bool,
    refresh: bool,
    visible: bool,
    stopping: bool,
    state: Option<RemoteState>,
    page: Option<RemotePage>,
}
type Shared = Arc<(Mutex<Mailbox>, Condvar)>;
struct QueuedControl {
    control: RemoteControl,
    owned_lane: Option<u64>,
    request_id: Option<u64>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResumeDispatchPhase {
    Preparing,
    Cancelled,
    Dispatched,
}
struct ResumeDispatch {
    operation: u64,
    device: String,
    phase: ResumeDispatchPhase,
}
impl ResumeDispatch {
    fn preparing(control: &RemoteControl) -> Option<Self> {
        let RemoteControl::ResumeOnDevice {
            operation_id: Some(operation),
            device_id,
            ..
        } = control
        else {
            return None;
        };
        Some(Self {
            operation: *operation,
            device: device_id.clone(),
            phase: ResumeDispatchPhase::Preparing,
        })
    }
    fn matches(&self, operation: u64, device: &str) -> bool {
        self.operation == operation && self.device == device
    }
    fn matches_control(&self, control: &RemoteControl) -> bool {
        matches!(control, RemoteControl::ResumeOnDevice { operation_id: Some(operation), device_id, .. }
            if self.matches(*operation, device_id))
    }
}
impl From<RemoteControl> for QueuedControl {
    fn from(control: RemoteControl) -> Self {
        Self {
            control,
            owned_lane: None,
            request_id: None,
        }
    }
}
struct DirectOwnedStart {
    operation: u64,
    generation: u64,
    device: String,
    observed_at: Option<Instant>,
    active_context_origin: Option<output_clock::ActiveContextStart>,
}
#[derive(Default)]
struct DeviceDiscovery {
    requested: bool,
    in_flight: bool,
    periodic_at: Option<Instant>,
    retry_at: Option<Instant>,
    empty_retries: usize,
}
impl DeviceDiscovery {
    const EMPTY_BACKOFF_MS: [u64; 3] = [750, 1500, 3000];

    fn start(&mut self, now: Instant) -> bool {
        if !self.requested && self.retry_at.is_none_or(|due| now < due) {
            return false;
        }
        self.requested = false;
        self.retry_at = None;
        self.in_flight = true;
        true
    }

    fn finish(&mut self, now: Instant, empty: Option<bool>) {
        self.in_flight = false;
        match empty {
            Some(false) => self.empty_retries = 0,
            Some(true) => {
                if let Some(delay) = Self::EMPTY_BACKOFF_MS.get(self.empty_retries) {
                    self.retry_at = Some(now + Duration::from_millis(*delay));
                    self.empty_retries += 1;
                }
            }
            None => (),
        }
    }
}
impl Mailbox {
    fn request_device_discovery(&mut self, now: Instant, explicit: bool) {
        let discovery = &self.device_discovery;
        if self.auxiliary_token.is_some()
            && !self.authorization_revoked
            && self.rate(now).is_none()
            && !discovery.requested
            && !discovery.in_flight
            && discovery.retry_at.is_none()
            && (explicit || discovery.periodic_at.is_none_or(|due| now >= due))
        {
            self.record_trace(autoplay_trace::Event::discovery(
                autoplay_trace::DiscoveryStage::Scheduled { explicit },
            ));
            self.device_discovery.requested = true;
            self.device_discovery.periodic_at =
                Some(now + Duration::from_secs(if self.visible { 15 } else { 60 }));
        }
    }
    fn cancel_owned_http(&mut self) {
        self.owned_http = None;
        self.controls.retain(|queued| queued.owned_lane.is_none());
    }
    fn recheck_owned_http(&mut self, frame: Option<&RemotePlayback>, now: Instant) {
        if let Some(lane) = &self.owned_http
            && lane.expired(now)
        {
            self.owned_http_issue |= lane.native_relative_pending()
                || self
                    .controls
                    .iter()
                    .any(|entry| entry.owned_lane == Some(lane.id));
        }
        if self.owned_http.as_ref().is_some_and(|lane| {
            self.authorization_revoked || lane.expired(now) || lane.conflicts_with(frame)
        }) {
            self.cancel_owned_http();
        }
    }
    fn update_owned_http_clock(&mut self, clock: Option<(&RemoteOutputClock, Instant)>) {
        let front_pause = self.front_owned_pause().is_some();
        let front_next = self.front_owned_next().is_some();
        let Some(lane) = &mut self.owned_http else {
            return;
        };
        if !front_pause {
            lane.discard_followup_offer(front_next);
        }
        if !lane.observe(clock) {
            self.owned_http_issue |= lane.waiting_to_send_previous()
                || lane.native_relative_pending()
                || lane.native_pause_pending()
                || lane.strict_followup_scope()
                    && self
                        .controls
                        .iter()
                        .any(|entry| entry.owned_lane == Some(lane.id));
            self.cancel_owned_http();
        }
    }
    fn advance_owned_seek(
        &mut self,
        commands: &owned_commands::OwnedCommands,
        clock: Option<&RemoteOutputClock>,
        now: Instant,
    ) -> bool {
        let Some(lane) = &mut self.owned_http else {
            return false;
        };
        let Some(seek) = lane.waiting_for_seek() else {
            return false;
        };
        if commands.seek_confirmed(seek, clock, now) {
            lane.confirm_seek(now);
        } else if !commands.waits_for_seek(seek, now) {
            self.cancel_owned_http();
            self.owned_http_issue = true;
        } else {
            return false;
        }
        true
    }
    fn cancel_owned_scoped_transition(&mut self) {
        if let Some(lane) = &self.owned_http
            && (lane.has_preceding_seek()
                || lane.strict_followup_scope()
                || lane.native_relative_pending())
        {
            self.owned_http_issue |= lane.native_pause_pending()
                || lane.native_relative_pending()
                || self
                    .controls
                    .iter()
                    .any(|entry| entry.owned_lane == Some(lane.id));
            self.cancel_owned_http();
        }
    }
    fn admit_owned_http(
        &mut self,
        control: &RemoteControl,
        output: &output_clock::OutputProjection,
        preceding_seek: Option<owned_commands::SeekBarrier>,
        now: Instant,
    ) -> Option<u64> {
        if !owned_http::follows_relative(control) {
            return None;
        }
        if let Some(lane) = &self.owned_http {
            return Some(lane.id);
        }
        if owned_http::relative(control) {
            let (clock, at) = output.http_navigation_clock(now)?;
            self.owned_http_serial = self.owned_http_serial.wrapping_add(1);
            self.owned_http = owned_http::Lane::new(
                self.owned_http_serial,
                clock,
                at,
                output.pending_http_row(clock),
                preceding_seek,
                now,
            );
        }
        self.owned_http.as_ref().map(|lane| lane.id)
    }
    fn front_owned_http(&self) -> Option<&owned_http::Lane> {
        let id = self.controls.front()?.owned_lane?;
        self.owned_http.as_ref().filter(|lane| lane.id == id)
    }
    fn front_owned_pause(&self) -> Option<&owned_http::Lane> {
        matches!(self.controls.front()?.control, RemoteControl::Pause)
            .then(|| self.front_owned_http())
            .flatten()
    }
    fn front_owned_previous(&self) -> Option<&owned_http::Lane> {
        matches!(self.controls.front()?.control, RemoteControl::Previous)
            .then(|| self.front_owned_http())
            .flatten()
    }
    fn front_owned_next(&self) -> Option<&owned_http::Lane> {
        matches!(self.controls.front()?.control, RemoteControl::Next)
            .then(|| self.front_owned_http())
            .flatten()
    }
    fn may_dispatch_owned_handoff(&self, now: Instant) -> bool {
        !self.authorization_revoked
            && self.control_issue(now).is_none()
            && self.rate(now).is_none()
            && self.auxiliary_failure.is_none()
    }
    fn may_dispatch_native_previous(&self) -> bool {
        !self.stopping
            && !self.authorization_revoked
            && self.auxiliary_token.is_some()
            && !self.http_mutating
            && self.resume_dispatch.is_none()
    }
    fn may_dispatch_previous_lane(&self, now: Instant) -> bool {
        self.may_dispatch_native_previous()
            && (self.may_dispatch_owned_handoff(now)
                || self
                    .owned_http
                    .as_ref()
                    .is_some_and(owned_http::Lane::native_only))
    }
    fn cued_previous_scope<'a>(
        &self,
        output: &'a output_clock::OutputProjection,
        now: Instant,
    ) -> Option<(&'a RemoteOutputClock, owned_http::PreviousWitness)> {
        let lane = self.front_owned_previous()?;
        let clock = output.control_clock(now)?;
        let witness = output.cued_previous_witness(now)?;
        lane.cued_previous_candidate(clock, &witness, now)
            .then_some((clock, witness))
    }
    fn native_next_target<'a>(
        &self,
        output: &'a output_clock::OutputProjection,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> Option<&'a MediaId> {
        let lane = self.front_owned_next()?;
        let head = output.next_context_head(now)?;
        lane.native_next_candidate(clock, head, now).then_some(head)
    }
    fn previous_resume_request(&self, clock: &RemoteOutputClock, now: Instant) -> Option<u64> {
        if self.stopping
            || self.http_mutating
            || !self.controls.is_empty()
            || self.auxiliary_token.is_none()
            || self.resume_dispatch.is_some()
        {
            return None;
        }
        self.owned_http
            .as_ref()?
            .previous_resume_request(clock, now)
    }
    fn owned_http_waiting(&self, now: Instant) -> bool {
        self.front_owned_http()
            .is_some_and(|lane| lane.native_only() || !lane.ready(now))
            || self
                .front_owned_pause()
                .is_some_and(|lane| lane.satisfied_cued_pause(now))
    }
    fn record_autoplay(&mut self, stage: autoplay_trace::Stage) {
        self.record_trace(autoplay_trace::Event::autoplay(stage));
    }
    fn record_trace(&mut self, event: Option<autoplay_trace::Event>) {
        if self.trace_events.len() < 16
            && let Some(event) = event
        {
            self.trace_events.push_back(event);
        }
    }
    fn prefers_direct_owned_start(&self, control: &RemoteControl) -> bool {
        let device = match control {
            RemoteControl::StartOnDevice { device_id, .. }
            | RemoteControl::StartContext {
                device_id,
                item: None,
                ..
            } => device_id,
            _ => return false,
        };
        self.direct_owned_start.as_ref().is_some_and(|start| {
            control.operation_id() == Some(start.operation) && *device == start.device
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct RateLimit {
    seconds: u64,
    developer_quota: bool,
}
impl RateLimit {
    fn issue(self) -> PlayerError {
        let message = if self.developer_quota {
            "The Spotify developer account has reached its Web API quota. Try again later.".into()
        } else {
            format!(
                "Spotify request limit reached. Try again in {} seconds.",
                self.seconds
            )
        };
        PlayerError::new(ErrorCode::RateLimited, message)
    }
}
fn failed_page(query: &RemoteQuery, error: PlayerError) -> RemotePage {
    RemotePage {
        position_edits: false,
        related_collections: vec![],
        detail: query.kind.is_detail(),
        item_positions: vec![],
        next_cursor: None,
        provider: "spotify".into(),
        query_id: query.query_id,
        offset: query.offset,
        total: 0,
        items: vec![],
        collections: vec![],
        spoken_progress: vec![],
        recent_plays: vec![],
        issue: Some(error),
    }
}
impl Mailbox {
    fn rate(&self, now: Instant) -> Option<RateLimit> {
        self.rate_limit
            .filter(|limit| limit.active(now))
            .map(|limit| RateLimit {
                seconds: limit.seconds(now),
                developer_quota: self.developer_quota,
            })
    }
    fn control_issue(&self, now: Instant) -> Option<PlayerError> {
        self.control_backoff
            .filter(|wait| wait.active(now))
            .and(self.control_backoff_issue.clone())
    }
    fn cancel_controls(&mut self, issue: &PlayerError, now: Instant) {
        let latest_operation = self
            .controls
            .iter()
            .rev()
            .find_map(|queued| queued.control.operation_id())
            .or_else(|| {
                self.cancelled_control_result
                    .as_ref()
                    .map(|r| r.operation_id)
            });
        if !self.authorization_revoked
            && self.auxiliary_token.is_some()
            && let Some(lane) = &mut self.owned_http
            && lane.can_keep_native_previous(now)
        {
            // Keep the proven native prefix, never work that needs an HTTP
            // retry or a Pause admitted behind some other queued transition.
            lane.restrict_to_native_previous();
            let mut prefix = true;
            let mut previous_pending = lane.waiting_to_send_previous();
            let mut pause_pending = true;
            self.controls.retain(|entry| {
                prefix &= entry.owned_lane == Some(lane.id)
                    && match entry.control {
                        RemoteControl::Previous if previous_pending => {
                            previous_pending = false;
                            true
                        }
                        RemoteControl::Pause if !previous_pending && pause_pending => {
                            pause_pending = false;
                            true
                        }
                        _ => false,
                    };
                prefix
            });
        } else {
            self.controls.clear();
            self.owned_http = None;
        }
        if let Some(operation_id) = latest_operation {
            self.cancelled_control_result = Some(RemoteControlResult {
                operation_id,
                error: Some(issue.clone()),
            });
        }
    }
    fn limit_requests(&mut self, seconds: u64, now: Instant) {
        if self.rate(now).is_none() {
            self.developer_quota = false;
        }
        Cooldown::extend(&mut self.rate_limit, now, seconds);
        let issue = RateLimit {
            seconds: self.rate(now).map_or(seconds, |limit| limit.seconds),
            developer_quota: self.developer_quota,
        }
        .issue();
        self.cancel_controls(&issue, now);
        if let Some(edit) = self.edit.take() {
            self.edit_result = Some(RemoteEditResult {
                operation_id: edit.operation_id,
                saved: vec![],
                collection: None,
                error: Some(issue.clone()),
            });
        }
        if let Some(query) = self.query.take() {
            self.page = Some(failed_page(&query, issue.clone()));
        }
        for query in self.details.drain(..) {
            self.detail_pages
                .push_back(failed_page(&query, issue.clone()));
        }
        self.refresh = false;
        self.device_discovery = DeviceDiscovery::default();
        self.request_queue = None;
        if let Some(request) = self.autoplay_request.take() {
            self.autoplay_result = Some((request.ticket(), None));
        }
    }
}
pub struct SpotifyRemoteProvider {
    output: output_clock::OutputProjection,
    owned_commands: owned_commands::OwnedCommands,
    autoplay: autoplay::Autoplay,
    autoplay_trace: Option<autoplay_trace::Trace>,
    shared: Shared,
    worker: Option<JoinHandle<()>>,
    auxiliary: Option<JoinHandle<()>>,
}
#[derive(Clone, PartialEq, Eq)]
struct QueueBinding {
    media: Option<MediaId>,
    device: Option<String>,
    context: Option<String>,
}
struct QueueRequest {
    binding: QueueBinding,
    epoch: u64,
}
impl QueueBinding {
    fn of(frame: &RemotePlayback) -> Self {
        Self {
            media: frame.current.as_ref().map(|item| item.id.clone()),
            device: frame.device.as_ref().map(|device| device.id.clone()),
            context: frame.context_uri.clone(),
        }
    }
}
impl SpotifyRemoteProvider {
    pub fn new() -> std::io::Result<Self> {
        Self::with_base("https://api.spotify.com/v1/".into())
    }
    fn with_base(base: String) -> std::io::Result<Self> {
        let shared = Arc::new((
            Mutex::new(Mailbox {
                visible: true,
                state: Some(disconnected(false)),
                ..Mailbox::default()
            }),
            Condvar::new(),
        ));
        let work = shared.clone();
        let worker = thread::Builder::new().name("auvra-spotify".into()).spawn({
            let base = base.clone();
            move || run(work, base)
        })?;
        let auxiliary = match thread::Builder::new()
            .name("auvra-spotify-catalog".into())
            .spawn({
                let shared = shared.clone();
                move || run_auxiliary(shared, base)
            }) {
            Ok(worker) => worker,
            Err(error) => {
                shared.0.lock().unwrap_or_else(|e| e.into_inner()).stopping = true;
                shared.1.notify_all();
                let _ = worker.join();
                return Err(error);
            }
        };
        Ok(Self {
            output: output_clock::OutputProjection::default(),
            owned_commands: owned_commands::OwnedCommands::default(),
            autoplay: autoplay::Autoplay::default(),
            autoplay_trace: autoplay_trace::enabled().then(autoplay_trace::Trace::default),
            shared,
            worker: Some(worker),
            auxiliary: Some(auxiliary),
        })
    }
}
fn disconnected(authentication_required: bool) -> RemoteState {
    RemoteState {
        owned_output_observed: false,
        owned_output_control: false,
        control_result: None,
        confirmed_same_media_next_request_id: None,
        observation_id: 0,
        provider: "spotify".into(),
        connection: ProviderConnectionState::Disconnected,
        authentication_required,
        issue: None,
        retry_after_seconds: None,
        devices: vec![],
        playback: None,
    }
}
fn invalid(message: &str) -> PlayerError {
    PlayerError::new(ErrorCode::InvalidCommand, message)
}
fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
impl SpotifyRemoteProvider {
    fn submit_action(
        &mut self,
        action: RemoteAction,
        previous_after_next_device: Option<&str>,
        request_id: Option<u64>,
    ) -> Result<(), PlayerError> {
        let mut box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if box_.stopping {
            return Err(PlayerError::unavailable("spotify"));
        }
        match action {
            RemoteAction::Credential { access_token } => {
                if access_token.is_empty()
                    || access_token.len() > 8192
                    || !access_token.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err(invalid("Invalid access credential"));
                }
                box_.generation = box_.generation.wrapping_add(1);
                self.output.reset();
                self.owned_commands.revoke();
                self.autoplay.clear();
                if let Some(trace) = &mut self.autoplay_trace {
                    trace.clear();
                }
                box_.autoplay_request = None;
                box_.autoplay_result = None;
                box_.direct_owned_start = None;
                box_.auxiliary_token = Some(access_token.clone());
                box_.authorization_revoked = false;
                box_.cancelled_control_result = None;
                box_.accepted_context_resume = None;
                box_.resume_dispatch = None;
                box_.control_backoff = None;
                box_.control_backoff_issue = None;
                box_.auxiliary_failure = None;
                box_.device_discovery = DeviceDiscovery::default();
                box_.request_queue = None;
                box_.local_queue_binding = None;
                box_.devices = None;
                box_.queue = None;
                box_.credential = Some(Some(access_token));
                box_.controls.clear();
                box_.owned_http = None;
                box_.owned_http_issue = false;
                box_.query = None;
                box_.latest_query_id = None;
                box_.context_page = None;
                box_.resume_album_tracks.clear();
                box_.details.clear();
                box_.detail_credits.clear();
                box_.detail_pages.clear();
                box_.edit = None;
                box_.edit_result = None;
                box_.edit_busy = false;
                box_.page = None;
                let mut state = disconnected(false);
                state.connection = ProviderConnectionState::Connecting;
                if let Some(limit) = box_.rate(Instant::now()) {
                    state.connection = ProviderConnectionState::Unavailable;
                    state.retry_after_seconds = Some(limit.seconds);
                    state.issue = Some(limit.issue());
                }
                box_.state = Some(state);
            }
            RemoteAction::Disconnect => {
                self.output.reset();
                self.owned_commands.revoke();
                self.autoplay.clear();
                if let Some(trace) = &mut self.autoplay_trace {
                    trace.clear();
                }
                box_.autoplay_request = None;
                box_.autoplay_result = None;
                box_.direct_owned_start = None;
                box_.generation = box_.generation.wrapping_add(1);
                box_.credential = Some(None);
                box_.auxiliary_token = None;
                box_.cancelled_control_result = None;
                box_.accepted_context_resume = None;
                box_.resume_dispatch = None;
                box_.control_backoff = None;
                box_.control_backoff_issue = None;
                box_.auxiliary_failure = None;
                box_.device_discovery = DeviceDiscovery::default();
                box_.request_queue = None;
                box_.local_queue_binding = None;
                box_.devices = None;
                box_.queue = None;
                box_.controls.clear();
                box_.owned_http = None;
                box_.owned_http_issue = false;
                box_.query = None;
                box_.latest_query_id = None;
                box_.context_page = None;
                box_.resume_album_tracks.clear();
                box_.details.clear();
                box_.detail_credits.clear();
                box_.detail_pages.clear();
                box_.edit = None;
                box_.edit_result = None;
                box_.edit_busy = false;
                box_.page = None;
                box_.state = Some(disconnected(false));
            }
            RemoteAction::AdoptOutput => {
                return Err(PlayerError::invalid(
                    "Output adoption belongs to the playback coordinator",
                ));
            }
            RemoteAction::Activate | RemoteAction::Refresh => {
                // Explicit retry may retry reads now; it never replays old actions
                // or bypasses the independent API-wide Retry-After window.
                box_.control_backoff = None;
                box_.control_backoff_issue = None;
                box_.refresh = true;
                box_.request_device_discovery(Instant::now(), true);
            }
            RemoteAction::OutputClock { clock } => {
                let now = Instant::now();
                let origin = self.autoplay_trace.as_ref().and_then(|trace| {
                    trace.origin(
                        clock.as_ref(),
                        self.output.autoplay_queue(now),
                        &self.autoplay,
                    )
                });
                if self.output.observe(clock, now)?
                    && (box_.http_mutating || !self.output.can_project_owned_metadata())
                {
                    // A bound track change already has fresh metadata. Leave
                    // ordinary HTTP polling and mutation verification in place.
                    box_.refresh = true;
                }
                box_.update_owned_http_clock(self.output.observed_at());
                if let Some(trace) = &mut self.autoplay_trace {
                    let clock = if self.output.has_owned_binding() {
                        self.output.observed_clock()
                    } else {
                        None
                    };
                    if let Some(stage) = trace.observe(clock, origin, now) {
                        box_.record_autoplay(stage);
                    }
                }
                if let Some(start) = &mut box_.direct_owned_start {
                    self.output.recheck_direct_owned_start(start);
                }
                let retained_pause = self.output.resume_pause_in_flight(now);
                if let Some((id, generation)) = self.output.revoked_resume_pause() {
                    self.owned_commands.revoke_resume_pause(id, generation);
                }
                if !self.output.has_owned_binding() {
                    box_.cancel_owned_scoped_transition();
                    self.owned_commands
                        .revoke_output_transport_except(retained_pause);
                    self.owned_commands.cancel_autoplay();
                    self.autoplay.lose_binding();
                }
                box_.advance_owned_seek(&self.owned_commands, self.output.control_clock(now), now);
            }
            RemoteAction::OutputQueue { queue } => {
                self.output.observe_queue(queue, Instant::now())?
            }
            RemoteAction::OutputCommandResult { result } => {
                let (id, generation, accepted) = (result.id, result.generation, result.accepted);
                self.output.resume_pause_in_flight(Instant::now());
                if let Some((id, generation)) = self.output.revoked_resume_pause() {
                    self.owned_commands.revoke_resume_pause(id, generation);
                }
                let scoped = self.owned_commands.awaiting_scoped_control(id, generation);
                let exact_pause = matches!(scoped, Some(RemoteOutputControl::Pause));
                let native_relative = matches!(
                    scoped,
                    Some(RemoteOutputControl::PreviousFromStart | RemoteOutputControl::Next)
                );
                match self.owned_commands.acknowledge(result) {
                    Some(
                        control @ (RemoteOutputControl::PreviousFromStart
                        | RemoteOutputControl::Next),
                    ) => {
                        if native_relative && let Some(lane) = &mut box_.owned_http {
                            lane.acknowledge_native_relative(id, generation, Instant::now());
                        }
                        if control == RemoteOutputControl::Next
                            && self.owned_commands.has_same_media_next_observation()
                        {
                            self.output.republish();
                        }
                    }
                    Some(RemoteOutputControl::StartUri { operation_id, .. }) => {
                        self.output
                            .set_local_control_result(Some(RemoteControlResult {
                                operation_id,
                                error: None,
                            }));
                    }
                    Some(RemoteOutputControl::Enqueue { .. })
                        if box_.local_queue_binding.is_none() =>
                    {
                        box_.refresh = true;
                    }
                    Some(RemoteOutputControl::AutoplayEnqueue { uri })
                        if self.autoplay.acknowledge(&uri) && self.autoplay_trace.is_some() =>
                    {
                        box_.record_autoplay(autoplay_trace::Stage::Acknowledged);
                    }
                    Some(RemoteOutputControl::Pause) if exact_pause => {
                        self.output.acknowledge_resume_pause(id, generation, true);
                        if let Some(lane) = &mut box_.owned_http {
                            lane.acknowledges_native_pause(id, generation, Instant::now());
                        }
                    }
                    _ => {}
                }
                if !accepted && exact_pause {
                    self.output.acknowledge_resume_pause(id, generation, false);
                }
                if !accepted
                    && box_.owned_http.as_ref().is_some_and(|lane| {
                        exact_pause && lane.owns_native_pause(id, generation)
                            || native_relative && lane.owns_native_relative(id, generation)
                    })
                {
                    box_.cancel_owned_http();
                }
                let now = Instant::now();
                box_.advance_owned_seek(&self.owned_commands, self.output.control_clock(now), now);
            }
            RemoteAction::Visibility { visible } => {
                box_.visible = visible;
                if visible {
                    box_.refresh = true;
                }
            }
            RemoteAction::Query { query } => {
                if query.text.len() > 512 || query.offset > 100_000 {
                    return Err(invalid("Catalog query is too large"));
                }
                use RemoteCatalogKind::*;
                if query.kind.is_home_shelf()
                    && (query.offset != 0
                        || (!matches!(query.kind, HomeArtistAlbums) && !query.text.is_empty()))
                {
                    return Err(invalid("Home shelves use a fixed first page"));
                }
                if matches!(query.kind, SearchAll) && query.offset > 1000 {
                    return Err(invalid("Invalid search offset"));
                }
                if matches!(query.kind, ArtistTopTracks | ArtistSelectedTracks) && query.offset != 0
                {
                    return Err(invalid("Artist track shelves use a fixed first page"));
                }
                if matches!(
                    query.kind,
                    PlaylistItems
                        | PlaylistInfo
                        | PlaylistImages
                        | AlbumTracks
                        | ArtistAlbums
                        | ArtistTopTracks
                        | ArtistSelectedTracks
                        | HomeArtistAlbums
                        | ShowEpisodes
                        | AudiobookChapters
                        | Album
                        | Artist
                        | Show
                        | Audiobook
                        | Track
                        | Episode
                        | Chapter
                ) && !mapping::valid_id(&query.text)
                {
                    return Err(invalid("Invalid catalog ID"));
                }
                if matches!(query.kind, TopTracks | TopArtists)
                    && !["", "short_term", "medium_term", "long_term"]
                        .contains(&query.text.as_str())
                {
                    return Err(invalid("Invalid listening period"));
                }
                if matches!(query.kind, FollowedArtists)
                    && !query.text.is_empty()
                    && !mapping::valid_id(&query.text)
                {
                    return Err(invalid("Invalid artist cursor"));
                }
                if matches!(query.kind, Recent)
                    && !query.text.is_empty()
                    && !query.text.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(invalid("Invalid history cursor"));
                }
                if query.kind.is_detail() {
                    if query.query_id == 0 || box_.detail_credits.contains(&query.query_id) {
                        return Err(invalid("Invalid or duplicate detail query"));
                    }
                    if box_.detail_credits.len() >= 8 {
                        return Err(PlayerError::new(
                            ErrorCode::RateLimited,
                            "Wait for the previous detail requests to finish",
                        ));
                    }
                    box_.detail_credits.insert(query.query_id);
                    if let Some(limit) = box_.rate(Instant::now()) {
                        box_.detail_pages
                            .push_back(failed_page(&query, limit.issue()));
                    } else {
                        box_.details.push_back(query);
                    }
                } else {
                    box_.latest_query_id = Some(query.query_id);
                    box_.context_page = None;
                    if let Some(limit) = box_.rate(Instant::now()) {
                        box_.page = Some(failed_page(&query, limit.issue()));
                    } else {
                        box_.query = Some(query);
                    }
                }
            }
            RemoteAction::Edit { request } => {
                validate_edit(&request)?;
                if let Some(limit) = box_.rate(Instant::now()) {
                    return Err(limit.issue());
                }
                if box_.edit_busy {
                    return Err(PlayerError::new(
                        ErrorCode::RateLimited,
                        "Wait for the previous library change to finish",
                    ));
                }
                box_.edit_busy = true;
                box_.edit = Some(request);
            }
            RemoteAction::Control { control } => {
                if control.operation_id() == Some(0) {
                    return Err(invalid("Invalid playback operation identity"));
                }
                match &control {
                    RemoteControl::StartContext {
                        uri,
                        device_id,
                        item,
                        position,
                        query_id,
                        ..
                    } => {
                        mapping::context_start_offset(uri, device_id, item.as_ref())?;
                        mapping::context_start_position(*position, *query_id, item.as_ref())?;
                        if item.is_some()
                            && (box_.auxiliary_token.is_none() || box_.authorization_revoked)
                        {
                            return Err(PlayerError::new(
                                ErrorCode::AuthenticationRequired,
                                "Sign in again to start playback",
                            ));
                        }
                    }

                    RemoteControl::Start { item } => {
                        mapping::track_uri(item)?;
                    }
                    RemoteControl::Enqueue { item } => {
                        mapping::queue_uri(item)?;
                    }
                    RemoteControl::StartOnDevice {
                        item, device_id, ..
                    } => {
                        mapping::track_uri(item)?;
                        if !valid_device_id(device_id) {
                            return Err(invalid("Invalid Connect device"));
                        }
                    }
                    RemoteControl::StartSpokenOnDevice {
                        item,
                        device_id,
                        position_ms,
                        ..
                    } => {
                        mapping::spoken_start_uri(item, *position_ms)?;
                        if !valid_device_id(device_id) {
                            return Err(invalid("Invalid Connect device"));
                        }
                        if box_.auxiliary_token.is_none() || box_.authorization_revoked {
                            return Err(PlayerError::new(
                                ErrorCode::AuthenticationRequired,
                                "Sign in again to continue playback",
                            ));
                        }
                    }
                    RemoteControl::ResumeOnDevice {
                        item,
                        device_id,
                        position_ms,
                        context_uri,
                        ..
                    } => {
                        mapping::track_uri(item)?;
                        if !valid_device_id(device_id)
                            || *position_ms > item.duration_ms
                            || context_uri
                                .as_deref()
                                .is_some_and(|uri| !mapping::valid_uri(uri))
                        {
                            return Err(invalid("Invalid saved position, context or device"));
                        }
                    }
                    RemoteControl::Volume { volume }
                        if !volume.is_finite() || !(0.0..=1.0).contains(volume) =>
                    {
                        return Err(invalid("Invalid remote volume"));
                    }
                    RemoteControl::Transfer { device_id, .. } if !valid_device_id(device_id) => {
                        return Err(invalid("Invalid Connect device"));
                    }
                    RemoteControl::PauseOnDevice { device_id } if !valid_device_id(device_id) => {
                        return Err(invalid("Invalid Connect device"));
                    }
                    RemoteControl::Stop => {
                        return Err(PlayerError::new(
                            ErrorCode::UnsupportedCapability,
                            "Spotify Connect supports Pause, not Stop",
                        ));
                    }
                    _ => {}
                }
                let now = Instant::now();
                if let Some(device) = previous_after_next_device
                    && (box_.authorization_revoked
                        || box_.http_mutating
                        || !box_.controls.is_empty()
                        || box_
                            .owned_http
                            .as_ref()
                            .is_some_and(owned_http::Lane::navigating)
                        || self.output.navigation_clock(now).is_none_or(|clock| {
                            clock.device_id != device
                                || !self.owned_commands.has_pending_next(clock, now)
                        }))
                {
                    return Err(owned_commands::unavailable());
                }
                if !matches!(control, RemoteControl::Volume { .. }) {
                    box_.playback_intent_serial = box_.playback_intent_serial.wrapping_add(1);
                    if let Some(lane) = &mut box_.owned_http {
                        lane.cancel_previous_resume();
                    }
                    if box_
                        .owned_http
                        .as_ref()
                        .is_some_and(owned_http::Lane::observing_previous)
                    {
                        box_.cancel_owned_http();
                    }
                    self.owned_commands.cancel_autoplay();
                }
                if control.operation_id().is_some() {
                    self.output.set_local_control_result(None);
                }
                let output_transport = matches!(
                    control,
                    RemoteControl::Next
                        | RemoteControl::Previous
                        | RemoteControl::Play
                        | RemoteControl::Pause
                );
                let clock = if output_transport {
                    self.output.navigation_clock(now)
                } else {
                    self.output.control_clock(now)
                };
                let http_pending = box_.http_mutating
                    || !box_.controls.is_empty()
                    || box_
                        .owned_http
                        .as_ref()
                        .is_some_and(owned_http::Lane::navigating);
                if !http_pending {
                    box_.owned_http = None;
                }
                let preceding_seek = if matches!(control, RemoteControl::Previous)
                    && previous_after_next_device.is_none()
                    && !http_pending
                {
                    self.output
                        .control_clock(now)
                        .and_then(|clock| self.owned_commands.seek_before_previous(clock, now))
                } else {
                    None
                };
                let targeted_previous =
                    if matches!(control, RemoteControl::Previous) && !http_pending {
                        self.output.control_clock(now).filter(|clock| {
                            preceding_seek.is_some()
                                || self.owned_commands.idle_for_autoplay()
                                    && !self.owned_commands.has_pending_output_transport(now)
                                    && owned_http::previous_prefers_web(clock)
                        })
                    } else {
                        None
                    };
                let cued_previous = targeted_previous.and_then(|clock| {
                    preceding_seek
                        .map_or(clock.position_ms < 4_000, |seek| {
                            seek.before_previous_restart()
                        })
                        .then(|| self.output.cued_previous_witness(now))
                        .flatten()
                });
                let verified = clock.is_some()
                    || (output_transport && box_.owned_http.is_some())
                    || (owned_http::relative(&control)
                        && self.output.http_navigation_clock(now).is_some());
                let record_route = |mailbox: &mut Mailbox, route, reason| {
                    mailbox.record_trace(autoplay_trace::Event::transport(
                        &control,
                        TransportStage::Route { route, reason },
                        verified,
                    ));
                };
                let unaddressed = matches!(
                    control,
                    RemoteControl::Play
                        | RemoteControl::Pause
                        | RemoteControl::Next
                        | RemoteControl::Previous
                        | RemoteControl::Seek { .. }
                        | RemoteControl::Volume { .. }
                        | RemoteControl::Shuffle { .. }
                        | RemoteControl::Repeat { .. }
                        | RemoteControl::Enqueue { .. }
                );
                if unaddressed
                    && clock.is_none()
                    && self.output.has_owned_destination_veto()
                    && !(owned_http::relative(&control)
                        && self.output.http_navigation_clock(now).is_some())
                {
                    record_route(&mut box_, Route::Rejected, RouteReason::StaleOwnedClock);
                    return Err(owned_commands::unavailable());
                }
                if !box_.authorization_revoked
                    && !http_pending
                    && targeted_previous.is_none()
                    && let Some(local) = owned_commands::convert(&control, clock)
                    && let Some(clock) = clock
                {
                    let use_owned = match local {
                        RemoteOutputControl::StartUri { .. } => true,
                        RemoteOutputControl::Next
                        | RemoteOutputControl::Previous
                        | RemoteOutputControl::Play
                        | RemoteOutputControl::Pause => {
                            // Wait for actions during a track change or an owned
                            // sequence of skips. A stable output may omit actions.
                            clock.actions.is_some()
                                || clock.state == PlaybackState::Loading
                                || self.owned_commands.has_pending_output_transport(now)
                        }
                        RemoteOutputControl::Volume { .. } => clock.volume.is_some(),
                        RemoteOutputControl::Seek { .. }
                        | RemoteOutputControl::Repeat { .. }
                        | RemoteOutputControl::Shuffle { .. } => {
                            clock.actions.is_some()
                                || self
                                    .owned_commands
                                    .waits_for_setting_actions(&local, clock, now)
                        }
                        _ => clock.actions.is_some(),
                    };
                    if use_owned {
                        let result = if matches!(control, RemoteControl::PauseOnDevice { .. }) {
                            self.owned_commands
                                .push_targeted_pause(clock, now, request_id)
                        } else {
                            self.owned_commands
                                .push_with_request_id(local, clock, now, request_id)
                        };
                        let (route, reason) = if result.is_ok() {
                            (Route::Owned, RouteReason::FreshClock)
                        } else {
                            (Route::Rejected, RouteReason::OwnedRejected)
                        };
                        record_route(&mut box_, route, reason);
                        if autoplay_trace::enabled() {
                            self.shared.1.notify_all();
                        }
                        if result.is_ok() {
                            self.output.remember_selection(&control, now);
                            self.autoplay.control(&control, now);
                            if let Some(trace) = &mut self.autoplay_trace {
                                trace.control(&control);
                            }
                        }
                        return result;
                    }
                }
                if previous_after_next_device.is_some() {
                    return Err(owned_commands::unavailable());
                }
                let rate = box_.rate(now);
                let backoff = box_.control_issue(now);
                let web_blocked = rate.is_some() || backoff.is_some();
                let native_only_previous = web_blocked
                    && box_.may_dispatch_native_previous()
                    && cued_previous
                        .as_ref()
                        .is_some_and(|witness| witness.unambiguous_context_predecessor);
                let previous_lane_native_only = box_
                    .owned_http
                    .as_ref()
                    .is_some_and(owned_http::Lane::native_only);
                let native_only_pause = (web_blocked || previous_lane_native_only)
                    && matches!(control, RemoteControl::Pause)
                    && box_.may_dispatch_native_previous()
                    && box_.owned_http.as_ref().is_some_and(|lane| {
                        lane.has_cued_previous(now)
                            && box_.controls.iter().all(|entry| {
                                entry.owned_lane == Some(lane.id)
                                    && matches!(
                                        entry.control,
                                        RemoteControl::Previous | RemoteControl::Pause
                                    )
                            })
                    });
                let native_only = native_only_previous || native_only_pause;
                if !native_only {
                    if let Some(limit) = rate {
                        record_route(&mut box_, Route::Rejected, RouteReason::RateLimited);
                        return Err(limit.issue());
                    }
                    if let Some(issue) = backoff {
                        record_route(&mut box_, Route::Rejected, RouteReason::Backoff);
                        return Err(issue);
                    }
                    if previous_lane_native_only && owned_http::follows_relative(&control) {
                        return Err(owned_commands::busy());
                    }
                }
                let waiting_for_seek = box_
                    .owned_http
                    .as_ref()
                    .is_some_and(|lane| lane.waiting_for_seek().is_some());
                let waiting_for_native_pause = box_
                    .owned_http
                    .as_ref()
                    .is_some_and(owned_http::Lane::native_pause_pending);
                let waiting_for_native_relative = box_
                    .owned_http
                    .as_ref()
                    .is_some_and(owned_http::Lane::native_relative_pending);
                let waiting_for_scoped_transport =
                    waiting_for_seek || waiting_for_native_pause || waiting_for_native_relative;
                let keep_order = preceding_seek.is_some()
                    || waiting_for_scoped_transport && owned_http::follows_relative(&control);
                if !keep_order
                    && (self.owned_commands.in_flight()
                        || waiting_for_scoped_transport && !owned_http::replaces_lane(&control))
                {
                    record_route(&mut box_, Route::Rejected, RouteReason::OwnedBusy);
                    return Err(owned_commands::busy());
                }
                if targeted_previous.is_some()
                    && (box_.authorization_revoked || box_.auxiliary_token.is_none())
                {
                    return Err(PlayerError::new(
                        ErrorCode::AuthenticationRequired,
                        "Reconnect your Spotify account.",
                    ));
                }
                if !keep_order {
                    self.owned_commands.revoke();
                }
                if owned_http::replaces_lane(&control) {
                    box_.cancel_owned_http();
                }
                // Keep the latest continuous setting/selection, preserving discrete skips
                // and queue additions. Rapid UI gestures must not accumulate seconds of IO.
                box_.controls
                    .retain(|old| !supersedes(&control, &old.control));
                if box_.controls.len() >= 32 {
                    record_route(&mut box_, Route::Rejected, RouteReason::QueueFull);
                    return Err(PlayerError::new(
                        ErrorCode::RateLimited,
                        "Remote command queue is full",
                    ));
                }
                if autoplay_trace::enabled() {
                    let reason = if targeted_previous.is_some() {
                        RouteReason::ExactPrevious
                    } else if http_pending {
                        RouteReason::AwaitingHttp
                    } else if !verified {
                        RouteReason::NoClock
                    } else if owned_commands::convert(&control, clock).is_some() {
                        RouteReason::NoActions
                    } else {
                        RouteReason::NotLocal
                    };
                    record_route(
                        &mut box_,
                        if native_only {
                            Route::Owned
                        } else {
                            Route::Http
                        },
                        reason,
                    );
                }
                let owned_lane = box_.admit_owned_http(&control, &self.output, preceding_seek, now);
                if let Some(clock) = targeted_previous {
                    let lane = box_
                        .owned_http
                        .as_mut()
                        .ok_or_else(owned_commands::unavailable)?;
                    lane.pin_previous(clock, self.output.previous_witness(now));
                    if let Some(witness) = cued_previous {
                        lane.offer_cued_previous(witness, now);
                    }
                }
                if native_only && let Some(lane) = &mut box_.owned_http {
                    lane.restrict_to_native_previous();
                }
                box_.advance_owned_seek(&self.owned_commands, self.output.control_clock(now), now);
                self.output.remember_selection(&control, now);
                if let Some(start) = self.output.remember_http_selection(&control, now) {
                    box_.direct_owned_start = Some(start);
                }
                self.autoplay.control(&control, now);
                if let Some(trace) = &mut self.autoplay_trace {
                    trace.control(&control);
                }
                box_.controls.push_back(QueuedControl {
                    request_id: request_id.filter(|_| {
                        matches!(control, RemoteControl::Next | RemoteControl::Previous)
                    }),
                    control,
                    owned_lane,
                });
            }
        }
        drop(box_);
        self.shared.1.notify_all();
        Ok(())
    }
}
impl RemoteProvider for SpotifyRemoteProvider {
    fn id(&self) -> &str {
        "spotify"
    }
    fn cancel_resume_before_dispatch(&mut self, operation_id: u64, device_id: &str) -> bool {
        if operation_id == 0 || !valid_device_id(device_id) {
            return false;
        }
        let mut box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if box_.stopping || box_.authorization_revoked {
            return false;
        }
        if let Some(resume) = box_
            .resume_dispatch
            .as_mut()
            .filter(|resume| resume.matches(operation_id, device_id))
        {
            if resume.phase == ResumeDispatchPhase::Dispatched {
                return false;
            }
            resume.phase = ResumeDispatchPhase::Cancelled;
        } else {
            let Some(index) = box_.controls.iter().position(|queued| {
                matches!(&queued.control, RemoteControl::ResumeOnDevice {
                    operation_id: Some(operation),
                    device_id: device,
                    ..
                } if *operation == operation_id && device == device_id)
            }) else {
                return false;
            };
            box_.controls.remove(index);
        }
        self.output.cancel_context_resume(operation_id, device_id);
        box_.record_trace(autoplay_trace::Event::discovery(
            autoplay_trace::DiscoveryStage::ResumeCancelledBeforeDispatch {
                operation: operation_id,
            },
        ));
        drop(box_);
        self.shared.1.notify_all();
        true
    }
    fn output_media_matches(&self, observed: &MediaId, catalog: &MediaId) -> bool {
        mapping::same_playback_id(observed, catalog)
    }
    fn owned_capture_witness(&self) -> Option<OwnedCaptureWitness> {
        let box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if box_.authorization_revoked {
            return None;
        }
        self.output.owned_capture_witness(Instant::now())
    }
    fn submit(&mut self, action: RemoteAction) -> Result<(), PlayerError> {
        self.submit_action(action, None, None)
    }
    fn submit_next_with_request_id(&mut self, request_id: u64) -> Result<(), PlayerError> {
        self.submit_action(
            RemoteAction::Control {
                control: RemoteControl::Next,
            },
            None,
            Some(request_id),
        )
    }
    fn owned_next_ready_for_request(&self, request_id: u64) -> bool {
        self.owned_commands.owned_next_ready_for_request(request_id)
    }
    fn submit_previous_with_request_id(&mut self, request_id: u64) -> Result<(), PlayerError> {
        self.submit_action(
            RemoteAction::Control {
                control: RemoteControl::Previous,
            },
            None,
            Some(request_id),
        )
    }
    fn submit_seek_with_request_id(
        &mut self,
        position_ms: u64,
        request_id: u64,
    ) -> Result<(), PlayerError> {
        self.submit_action(
            RemoteAction::Control {
                control: RemoteControl::Seek { position_ms },
            },
            None,
            Some(request_id),
        )
    }
    fn owned_seek_ready_for_request(&self, request_id: u64) -> bool {
        self.owned_commands.owned_seek_ready_for_request(request_id)
    }
    fn submit_play_pause_with_request_id(
        &mut self,
        play: bool,
        request_id: u64,
    ) -> Result<(), PlayerError> {
        self.submit_action(
            RemoteAction::Control {
                control: if play {
                    RemoteControl::Play
                } else {
                    RemoteControl::Pause
                },
            },
            None,
            Some(request_id),
        )
    }
    fn owned_play_pause_ready_for_request(&self, request_id: u64) -> bool {
        self.output.resume_pause_ready(request_id)
            || self
                .owned_commands
                .owned_play_pause_ready_for_request(request_id)
    }
    fn submit_pause_on_device_with_request_id(
        &mut self,
        device_id: &str,
        request_id: u64,
    ) -> Result<(), PlayerError> {
        self.submit_action(
            RemoteAction::Control {
                control: RemoteControl::PauseOnDevice {
                    device_id: device_id.into(),
                },
            },
            None,
            Some(request_id),
        )
    }
    fn submit_owned_pause_after_resume(
        &mut self,
        operation_id: u64,
        device_id: &str,
        request_id: u64,
    ) -> Result<bool, PlayerError> {
        if operation_id == 0 || request_id == 0 || !valid_device_id(device_id) {
            return Err(invalid("Invalid resumed playback request"));
        }
        let now = Instant::now();
        let box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if box_.authorization_revoked || box_.stopping {
            return Err(owned_commands::unavailable());
        }
        let Some(clock) = self
            .output
            .resume_pause_clock(operation_id, device_id, now)?
        else {
            return Ok(false);
        };
        if !box_.may_dispatch_owned_handoff(now)
            || !box_.controls.is_empty()
            || box_.owned_http.is_some()
            || box_.resume_dispatch.is_some()
        {
            return Err(owned_commands::unavailable());
        }
        let Some(command) = self.owned_commands.dispatch_scoped(
            RemoteOutputControl::Pause,
            clock,
            now,
            Some(request_id),
        ) else {
            return Err(owned_commands::unavailable());
        };
        self.output.reserve_resume_pause(command, now);
        Ok(true)
    }
    fn owned_handoff_ready(&self) -> bool {
        let now = Instant::now();
        let box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if !box_.authorization_revoked
            && !box_.http_mutating
            && box_.controls.is_empty()
            && (if self.owned_commands.next_requires_control_clock() {
                self.output.control_clock(now)
            } else {
                self.output.navigation_clock(now)
            })
            .is_some_and(|clock| {
                self.owned_commands.targeted_pause_can_dispatch(clock)
                    && self.owned_commands.owned_followup_ready(clock, now)
            })
        {
            return true;
        }
        let previous_lane = box_.may_dispatch_previous_lane(now);
        let normal_handoff = box_.may_dispatch_owned_handoff(now);
        (previous_lane
            && box_
                .cued_previous_scope(&self.output, now)
                .is_some_and(|(clock, _)| {
                    self.owned_commands
                        .scoped_dispatch_ready(&RemoteOutputControl::PreviousFromStart, clock)
                }))
            // The verified new clock can arrive before its state is projected.
            // The bounded poll still publishes that state before any dispatch.
            || self.output.navigation_clock(now).is_some_and(|clock| {
                (normal_handoff && box_.native_next_target(&self.output, clock, now).is_some()
                        && self.owned_commands.scoped_dispatch_ready(&RemoteOutputControl::Next, clock))
                    || ((previous_lane || normal_handoff)
                        && box_.front_owned_pause().is_some_and(|lane| lane.native_pause_candidate(clock, now))
                        && self.owned_commands.scoped_dispatch_ready(&RemoteOutputControl::Pause, clock))
                    || (previous_lane && box_.previous_resume_request(clock, now).is_some()
                        && self.owned_commands.scoped_dispatch_ready(&RemoteOutputControl::Play, clock))
                    || (!self.owned_commands.in_flight()
                        && box_.may_dispatch_native_previous()
                        && box_.front_owned_pause().is_some_and(|lane| lane.satisfies_cued_pause_on(clock, now)))
            })
    }
    fn submit_previous_after_next(&mut self, device_id: &str) -> Result<(), PlayerError> {
        self.submit_action(
            RemoteAction::Control {
                control: RemoteControl::Previous,
            },
            Some(device_id),
            None,
        )
    }
    fn poll(&mut self) -> Option<RemoteUpdate> {
        let mut box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        self.output.set_rate_limit(box_.rate(Instant::now()));
        if let Some(state) = box_.state.take() {
            box_.recheck_owned_http(state.playback.as_ref(), Instant::now());
            if let Some(receipt) = box_.accepted_context_resume.take() {
                self.output.accept_context_resume(receipt);
            }
            self.output.ingest(state, Instant::now());
            if let Some(start) = &mut box_.direct_owned_start {
                self.output.recheck_direct_owned_start(start);
            }
            let retained_pause = self.output.resume_pause_in_flight(Instant::now());
            if let Some((id, generation)) = self.output.revoked_resume_pause() {
                self.owned_commands.revoke_resume_pause(id, generation);
            }
            if !self.output.has_owned_binding() {
                box_.cancel_owned_scoped_transition();
                self.owned_commands
                    .revoke_output_transport_except(retained_pause);
                self.owned_commands.cancel_autoplay();
                self.autoplay.lose_binding();
                if let Some(trace) = &mut self.autoplay_trace {
                    trace.clear();
                }
            }
        }
        if box_.authorization_revoked {
            self.output.revoke_resume_pause_proof();
            self.owned_commands.revoke();
            self.autoplay.clear();
        }
        let now = Instant::now();
        if let Some((ticket, page)) = box_.autoplay_result.take() {
            self.autoplay.receive(ticket, page);
        }
        if let Some(mut state) = self.output.take(now) {
            let clock = self.output.control_clock(now);
            self.owned_commands.observe_projected_playback(clock, now);
            state.confirmed_same_media_next_request_id =
                self.owned_commands.project_same_media_next(clock, now);
            let local = state
                .playback
                .as_ref()
                .filter(|p| p.device_queue.is_some() && self.output.queue_has_titles())
                .map(QueueBinding::of);
            if box_.local_queue_binding.is_some()
                && local.is_none()
                && state.connection == ProviderConnectionState::Connected
            {
                box_.refresh = true;
                self.shared.1.notify_all();
            }
            if box_
                .request_queue
                .as_ref()
                .is_some_and(|request| Some(&request.binding) == local.as_ref())
            {
                box_.request_queue = None;
            }
            box_.local_queue_binding = local;
            if let Some(playback) = state.playback.as_mut()
                && let Some(queue) = playback.device_queue.as_mut()
            {
                for (item, source) in playback.upcoming.iter().zip(&mut queue.upcoming_sources) {
                    if *source == Some(QueueEntrySource::Queue)
                        && self.autoplay.is_offered(&item.id)
                    {
                        *source = Some(QueueEntrySource::Autoplay);
                    }
                }
            }
            return Some(RemoteUpdate::State(Box::new(state)));
        }
        if !box_.may_dispatch_owned_handoff(now)
            || !box_.controls.is_empty()
            || box_.owned_http.is_some()
            || box_.resume_dispatch.is_some()
        {
            self.output.revoke_resume_pause_proof();
        }
        if let Some(update) = self.output.take_resume_pause(now) {
            if matches!(&update, RemoteUpdate::OutputCommandFailed { .. })
                && let Some((id, generation)) = self.output.revoked_resume_pause()
            {
                // This reserved command never reached the helper.
                self.owned_commands.revoke_resume_pause(id, generation);
                self.owned_commands.acknowledge(RemoteOutputCommandResult {
                    id,
                    generation,
                    accepted: false,
                });
            }
            return Some(update);
        }
        let now = Instant::now();
        let previous_lane = box_.may_dispatch_previous_lane(now);
        let normal_handoff = box_.may_dispatch_owned_handoff(now);
        if previous_lane
            && let Some((clock, witness)) = box_.cued_previous_scope(&self.output, now)
            && let Some(mut command) = self.owned_commands.dispatch_previous_from_start(
                clock,
                now,
                box_.controls.front().and_then(|entry| entry.request_id),
            )
        {
            // The host waits for the scoped ACK and successor clock. Repeated
            // history entries can still identify the same pinned audio item.
            command.expected_previous_media_id = Some(witness.predecessor);
            box_.controls.pop_front();
            let resume_request = command.request_id.filter(|_| box_.controls.is_empty());
            if let Some(lane) = &mut box_.owned_http {
                lane.begin_cued_previous(command.id, resume_request, now);
            }
            box_.record_trace(autoplay_trace::Event::transport(
                &RemoteControl::Previous,
                TransportStage::OwnedPreviousFromStart,
                true,
            ));
            self.shared.1.notify_all();
            return Some(RemoteUpdate::OutputCommand(command));
        }
        if !self.owned_commands.in_flight()
            && box_.may_dispatch_native_previous()
            && let Some(clock) = self.output.control_clock(now)
            && box_
                .front_owned_pause()
                .is_some_and(|lane| lane.satisfies_cued_pause_on(clock, now))
        {
            box_.controls.pop_front();
            if let Some(lane) = &mut box_.owned_http {
                lane.discard_followup_offer(false);
            }
            box_.record_trace(autoplay_trace::Event::transport(
                &RemoteControl::Pause,
                TransportStage::OwnedPreviousSuccessorAlreadyPaused,
                true,
            ));
            self.shared.1.notify_all();
        }
        if previous_lane
            && let Some(clock) = self.output.control_clock(now)
            && let Some(request_id) = box_.previous_resume_request(clock, now)
            && let Some(command) = self.owned_commands.dispatch_scoped(
                RemoteOutputControl::Play,
                clock,
                now,
                Some(request_id),
            )
        {
            if let Some(lane) = &mut box_.owned_http {
                lane.cancel_previous_resume();
            }
            box_.record_trace(autoplay_trace::Event::transport(
                &RemoteControl::Play,
                TransportStage::OwnedPreviousSuccessorPlay,
                true,
            ));
            self.shared.1.notify_all();
            return Some(RemoteUpdate::OutputCommand(command));
        }
        if (previous_lane || normal_handoff)
            && let Some(clock) = self.output.control_clock(now)
            && box_
                .front_owned_pause()
                .is_some_and(|lane| lane.native_pause_candidate(clock, now))
            && let Some(command) = self.owned_commands.dispatch_exact_pause(clock, now)
        {
            box_.controls.pop_front();
            let mut stage = TransportStage::OwnedPreviousSuccessorPause;
            if let Some(lane) = &mut box_.owned_http {
                if lane.pause_follows_native_next() {
                    stage = TransportStage::OwnedNextSuccessorPause;
                }
                lane.begin_native_pause(command.id, now);
            }
            box_.record_trace(autoplay_trace::Event::transport(
                &RemoteControl::Pause,
                stage,
                true,
            ));
            self.shared.1.notify_all();
            return Some(RemoteUpdate::OutputCommand(command));
        }
        if normal_handoff
            && let Some(clock) = self.output.control_clock(now)
            && let Some(target) = box_.native_next_target(&self.output, clock, now)
            && let Some(command) = self.owned_commands.dispatch_scoped(
                RemoteOutputControl::Next,
                clock,
                now,
                box_.controls.front().and_then(|entry| entry.request_id),
            )
        {
            box_.controls.pop_front();
            if let Some(lane) = &mut box_.owned_http {
                lane.begin_native_next(command.id, target.clone(), now);
            }
            box_.record_trace(autoplay_trace::Event::transport(
                &RemoteControl::Next,
                TransportStage::OwnedNextAfterPrevious,
                true,
            ));
            self.shared.1.notify_all();
            return Some(RemoteUpdate::OutputCommand(command));
        }
        let dispatch_seek = box_
            .front_owned_http()
            .and_then(owned_http::Lane::waiting_for_seek)
            .is_some_and(|seek| self.owned_commands.queued_seek(seek));
        let control_clock = if box_.authorization_revoked
            || box_.http_mutating
            || !box_.controls.is_empty() && !dispatch_seek
        {
            None
        } else if self.owned_commands.next_requires_control_clock() {
            self.output.control_clock(now)
        } else if self.owned_commands.next_is_output_transport() {
            self.output.navigation_clock(now)
        } else {
            self.output.control_clock(now)
        };
        if self.owned_commands.idle_for_autoplay()
            && let Some(clock) = control_clock
        {
            let queue = self.output.autoplay_queue(now);
            self.autoplay.observe(clock, queue, now);
            if box_.autoplay_request.is_none()
                && box_.auxiliary_token.is_some()
                && box_.rate(now).is_none()
                && clock
                    .options
                    .as_ref()
                    .is_some_and(|options| options.repeat == RepeatMode::Off)
                && queue.is_some_and(|(queue, _)| {
                    queue.upcoming.len() <= 1
                        && !queue
                            .upcoming
                            .iter()
                            .any(|row| row.source == Some(QueueEntrySource::Autoplay))
                })
                && let Some(request) = self.autoplay.request(now)
            {
                box_.autoplay_request = Some(request);
                self.shared.1.notify_all();
            }
            if let Some((queue, age)) = queue
                && let Some(clock_age) = self.output.control_clock_age(now)
                && let Some(id) = self.autoplay.take_candidate(clock, queue, age, clock_age)
            {
                let queued = self.owned_commands.push(
                    RemoteOutputControl::AutoplayEnqueue {
                        uri: id.as_str().into(),
                    },
                    clock,
                    now,
                );
                if queued.is_ok() && self.autoplay_trace.is_some() {
                    box_.record_autoplay(autoplay_trace::Stage::Offered);
                    self.shared.1.notify_all();
                }
            }
        }
        let update = self.owned_commands.poll(control_clock, now);
        if box_.advance_owned_seek(&self.owned_commands, self.output.control_clock(now), now) {
            self.shared.1.notify_all();
        }
        if let Some(update) = update {
            if matches!(
                &update,
                RemoteUpdate::OutputCommandFailed {
                    control: RemoteOutputControl::Pause,
                    ..
                }
            ) && box_
                .owned_http
                .as_ref()
                .is_some_and(owned_http::Lane::native_pause_pending)
            {
                box_.cancel_owned_http();
                self.shared.1.notify_all();
            }
            if matches!(
                &update,
                RemoteUpdate::OutputCommandFailed {
                    control: RemoteOutputControl::PreviousFromStart | RemoteOutputControl::Next,
                    ..
                }
            ) && box_
                .owned_http
                .as_ref()
                .is_some_and(owned_http::Lane::native_relative_pending)
            {
                box_.cancel_owned_http();
                self.shared.1.notify_all();
            }
            if let RemoteUpdate::OutputCommandFailed {
                control: RemoteOutputControl::StartUri { operation_id, .. },
                error,
            } = update
            {
                self.output
                    .set_local_control_result(Some(RemoteControlResult {
                        operation_id,
                        error: Some(error),
                    }));
                return self
                    .output
                    .take(now)
                    .map(|state| RemoteUpdate::State(Box::new(state)));
            }
            return Some(update);
        }
        if let Some(result) = box_.edit_result.take() {
            box_.edit_busy = false;
            return Some(RemoteUpdate::Edited(result));
        }
        if let Some(page) = box_.page.take() {
            self.output.remember_catalog(&page);
            return Some(RemoteUpdate::Page(page));
        }
        box_.detail_pages.pop_front().map(|page| {
            box_.detail_credits.remove(&page.query_id);
            self.output.remember_catalog(&page);
            RemoteUpdate::Page(page)
        })
    }
    fn has_pending_output_start(&self) -> bool {
        if self.owned_commands.has_pending_start()
            || self
                .owned_commands
                .has_pending_silent_activation(Instant::now())
        {
            return true;
        }
        let box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        box_.http_starting_targeted_item
            || box_
                .owned_http
                .as_ref()
                .is_some_and(|lane| lane.relative_start_pending(Instant::now()))
            || box_.controls.iter().any(|control| {
                starts_targeted_item(&control.control)
                    || box_.prefers_direct_owned_start(&control.control)
            })
    }
}
impl Drop for SpotifyRemoteProvider {
    fn drop(&mut self) {
        {
            let mut box_ = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
            box_.stopping = true;
            box_.credential = None;
            box_.auxiliary_token = None;
            box_.controls.clear();
            box_.query = None;
            box_.details.clear();
            box_.detail_credits.clear();
            box_.detail_pages.clear();
        }
        self.shared.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.auxiliary.take() {
            let _ = worker.join();
        }
    }
}
fn supersedes(new: &RemoteControl, old: &RemoteControl) -> bool {
    if let (
        RemoteControl::PauseOnDevice { device_id: target },
        RemoteControl::ResumeOnDevice { device_id, .. }
        | RemoteControl::StartSpokenOnDevice { device_id, .. }
        | RemoteControl::StartContext {
            item: Some(_),
            device_id,
            ..
        },
    ) = (new, old)
    {
        return target == device_id;
    }
    matches!(
        (new, old),
        (RemoteControl::Volume { .. }, RemoteControl::Volume { .. })
            | (RemoteControl::Seek { .. }, RemoteControl::Seek { .. })
            | (RemoteControl::Shuffle { .. }, RemoteControl::Shuffle { .. })
            | (RemoteControl::Repeat { .. }, RemoteControl::Repeat { .. })
            | (
                RemoteControl::Transfer {
                    operation_id: Some(_),
                    ..
                },
                RemoteControl::Transfer { .. }
            )
            | (
                RemoteControl::Transfer { .. },
                RemoteControl::ResumeOnDevice { .. }
                    | RemoteControl::StartContext { item: Some(_), .. }
            )
            | (
                RemoteControl::Play | RemoteControl::Pause,
                RemoteControl::Play | RemoteControl::Pause
            )
            | (
                RemoteControl::Start { .. }
                    | RemoteControl::StartOnDevice { .. }
                    | RemoteControl::StartSpokenOnDevice { .. }
                    | RemoteControl::ResumeOnDevice { .. }
                    | RemoteControl::StartContext { .. },
                RemoteControl::Start { .. }
                    | RemoteControl::StartOnDevice { .. }
                    | RemoteControl::StartSpokenOnDevice { .. }
                    | RemoteControl::ResumeOnDevice { .. }
                    | RemoteControl::StartContext { .. }
                    | RemoteControl::Play
                    | RemoteControl::Pause
                    | RemoteControl::Seek { .. }
            )
            | (
                RemoteControl::Pause,
                RemoteControl::Start { .. }
                    | RemoteControl::StartOnDevice { .. }
                    | RemoteControl::StartSpokenOnDevice { .. }
                    | RemoteControl::ResumeOnDevice { .. }
                    | RemoteControl::StartContext { .. }
            )
    )
}
fn starts_targeted_item(control: &RemoteControl) -> bool {
    matches!(
        control,
        RemoteControl::StartSpokenOnDevice { .. }
            | RemoteControl::StartContext { item: Some(_), .. }
    )
}
fn publish(shared: &Shared, generation: u64, update: RemoteUpdate) {
    let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
    if box_.generation != generation
        || box_.stopping
        || box_.authorization_revoked
            && !matches!(&update, RemoteUpdate::State(state) if state.authentication_required)
    {
        return;
    }
    match update {
        RemoteUpdate::State(state) => box_.state = Some(*state),
        RemoteUpdate::Page(page) => {
            if page.detail {
                if box_.detail_credits.contains(&page.query_id) {
                    box_.detail_pages
                        .retain(|old| old.query_id != page.query_id);
                    box_.detail_pages.push_back(page);
                }
            } else if box_.latest_query_id.is_none_or(|id| id == page.query_id) {
                box_.page = Some(page);
            }
        }
        RemoteUpdate::Edited(result) => box_.edit_result = Some(result),
        RemoteUpdate::OutputCommand(_) | RemoteUpdate::OutputCommandFailed { .. } => {
            unreachable!("Owned commands do not use HTTP workers")
        }
    }
}
struct ControlConfirmation {
    control: RemoteControl,
    previous: Option<(MediaId, u64)>,
    until: Instant,
    attempts: u8,
}
impl ControlConfirmation {
    fn positioned_row_device(&self) -> Option<&str> {
        match &self.control {
            RemoteControl::StartContext {
                device_id,
                item: Some(_),
                position: Some(_),
                query_id: Some(_),
                ..
            } => Some(device_id),
            _ => None,
        }
    }
    fn selects_output_or_context(&self) -> bool {
        matches!(
            self.control,
            RemoteControl::Start { .. }
                | RemoteControl::StartOnDevice { .. }
                | RemoteControl::StartSpokenOnDevice { .. }
                | RemoteControl::StartContext { .. }
                | RemoteControl::ResumeOnDevice { .. }
                | RemoteControl::Transfer { .. }
        )
    }
    fn matches(&self, frame: Option<&RemotePlayback>) -> bool {
        let Some(frame) = frame else {
            return matches!(
                self.control,
                RemoteControl::Pause | RemoteControl::PauseOnDevice { .. }
            );
        };
        match &self.control {
            RemoteControl::StartOnDevice {
                item, device_id, ..
            }
            | RemoteControl::StartSpokenOnDevice {
                item, device_id, ..
            }
            | RemoteControl::ResumeOnDevice {
                item, device_id, ..
            } => {
                frame.state == PlaybackState::Playing
                    && frame
                        .current
                        .as_ref()
                        .is_some_and(|current| mapping::same_playback_id(&current.id, &item.id))
                    && frame
                        .device
                        .as_ref()
                        .is_some_and(|device| device.id == *device_id)
                    && match &self.control {
                        RemoteControl::ResumeOnDevice { position_ms, .. }
                        | RemoteControl::StartSpokenOnDevice { position_ms, .. } => {
                            frame.position_ms.abs_diff(*position_ms) <= 10_000
                        }
                        _ => true,
                    }
            }
            RemoteControl::Start { item } => {
                frame.state == PlaybackState::Playing
                    && frame
                        .current
                        .as_ref()
                        .is_some_and(|current| mapping::same_playback_id(&current.id, &item.id))
            }
            RemoteControl::StartContext {
                uri,
                device_id,
                item,
                ..
            } => {
                frame.context_uri.as_deref() == Some(uri.as_str())
                    && frame.state == PlaybackState::Playing
                    && frame.device.as_ref().is_some_and(|device| {
                        device.id == *device_id
                            && (item.is_none() || device.is_active && !device.is_restricted)
                    })
                    && item.as_ref().is_none_or(|item| {
                        frame
                            .current
                            .as_ref()
                            .is_some_and(|current| current.id == item.id)
                            && frame.position_ms <= 10_000
                    })
            }
            RemoteControl::Play => frame.state == PlaybackState::Playing,
            RemoteControl::Pause => frame.state == PlaybackState::Paused,
            RemoteControl::PauseOnDevice { device_id } => {
                frame.device.as_ref().is_none_or(|d| d.id != *device_id)
                    || frame.state == PlaybackState::Paused
            }
            RemoteControl::Next | RemoteControl::Previous => {
                self.previous.as_ref().is_none_or(|(id, position)| {
                    frame
                        .current
                        .as_ref()
                        .is_none_or(|current| current.id != *id)
                        || frame.position_ms.saturating_add(1000) < *position
                })
            }
            RemoteControl::Seek { position_ms } => frame.position_ms.abs_diff(*position_ms) < 1500,
            RemoteControl::Volume { volume } => (frame.volume - volume).abs() < 0.015,
            RemoteControl::Shuffle { enabled } => frame.shuffle == *enabled,
            RemoteControl::Repeat { mode } => frame.repeat == *mode,
            RemoteControl::Transfer { device_id, .. } => {
                frame.device.as_ref().is_some_and(|device| {
                    device.id == *device_id && device.is_active && !device.is_restricted
                })
            }
            RemoteControl::Enqueue { .. } | RemoteControl::Stop => true,
        }
    }
}
struct PreviousObservation {
    lane: u64,
    intent: u64,
    deadline: Instant,
    issue: PlayerError,
}

fn run(shared: Shared, base: String) {
    let Ok(mut api) = Api::new(base, shared.clone()) else {
        return;
    };
    let mut state = disconnected(false);
    let mut generation = 0;
    let mut observation_id = 0_u64;
    let mut next_poll = Instant::now();
    let mut next_queue = Instant::now();
    let mut queue_epoch = 0_u64;
    let mut failures = 0_u32;
    let mut command_issue = None;
    let mut confirmation: Option<ControlConfirmation> = None;
    let mut previous_observation: Option<PreviousObservation> = None;
    let mut read_deferred_for_row = false;
    loop {
        if autoplay_trace::enabled() {
            let events = {
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut box_.trace_events)
            };
            for event in events {
                event.write();
            }
        }
        let (control, direct_owned_start, visible, refresh, auxiliary_failure) = {
            let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
            if box_.stopping {
                break;
            }
            if let Some(credential) = box_.credential.take() {
                box_.http_mutating = false;
                box_.http_starting_targeted_item = false;
                api.token = credential;
                generation = box_.generation;
                api.generation = generation;
                state = disconnected(false);
                command_issue = None;
                confirmation = None;
                previous_observation = None;
                read_deferred_for_row = false;
                failures = 0;
                next_poll = Instant::now();
                next_queue = Instant::now();
                state.connection = if api.token.is_some() {
                    ProviderConnectionState::Connecting
                } else {
                    ProviderConnectionState::Disconnected
                };
                if api.token.is_some()
                    && let Some(limit) = box_.rate(Instant::now())
                {
                    state.connection = ProviderConnectionState::Unavailable;
                    state.retry_after_seconds = Some(limit.seconds);
                    state.issue = Some(limit.issue());
                }
                box_.state = Some(state.clone());
            }
            let mut changed = false;
            if let Some(devices) = box_.devices.take() {
                box_.record_trace(autoplay_trace::Event::discovery(
                    autoplay_trace::DiscoveryStage::Merged {
                        count: devices.len(),
                    },
                ));
                state.devices = devices;
                changed = true;
            }
            if let Some((request, queue)) = box_.queue.take()
                && request.epoch == queue_epoch
                && !box_.authorization_revoked
                && let Some(frame) = state.playback.as_mut()
                && QueueBinding::of(frame) == request.binding
            {
                frame.upcoming = queue;
                changed = true;
            }
            // Supplementary results retain observation_id: they must not reset the
            // playback clock to the older HTTP position anchor.
            if changed {
                box_.state = Some(state.clone());
            }
            let auxiliary_failure = box_.auxiliary_failure.take();
            if confirmation
                .as_ref()
                .is_some_and(|pending| Instant::now() >= pending.until)
            {
                confirmation = None;
                box_.http_mutating = false;
                box_.http_starting_targeted_item = false;
            }
            let now = Instant::now();
            let blocked = box_.control_issue(now).is_some() || box_.rate(now).is_some();
            if let Some(pending) = &previous_observation {
                let lane = box_
                    .owned_http
                    .as_ref()
                    .filter(|lane| lane.id == pending.lane);
                let superseded = box_.playback_intent_serial != pending.intent
                    || box_.authorization_revoked
                    || box_.rate(now).is_some()
                    || auxiliary_failure.is_some();
                let observed = !superseded
                    && now < pending.deadline
                    && lane.is_some_and(|lane| lane.previous_observed(now));
                let volume_ready = api.token.is_some()
                    && !blocked
                    && auxiliary_failure.is_none()
                    && box_.controls.front().is_some_and(|queued| {
                        queued.owned_lane.is_none()
                            && matches!(queued.control, RemoteControl::Volume { .. })
                    });
                if observed
                    || superseded
                    || volume_ready
                    || now >= pending.deadline
                    || lane.is_none_or(|lane| !lane.can_reconcile_previous())
                {
                    if observed {
                        box_.record_trace(autoplay_trace::Event::transport(
                            &RemoteControl::Previous,
                            TransportStage::PreviousObservedAfterTimeout,
                            true,
                        ));
                    } else if !superseded {
                        // Resolve the observation before a volume request can block this worker.
                        state.issue = Some(pending.issue.clone());
                        command_issue = Some(pending.issue.clone());
                        box_.state = Some(state.clone());
                    }
                    if box_
                        .owned_http
                        .as_ref()
                        .is_some_and(|lane| lane.id == pending.lane)
                    {
                        box_.cancel_owned_http();
                    }
                    previous_observation = None;
                }
            }
            box_.recheck_owned_http(state.playback.as_ref(), now);
            if std::mem::take(&mut box_.owned_http_issue) {
                let issue = PlayerError::new(
                    ErrorCode::InvalidCommand,
                    "Spotify playback changed or could not be confirmed. Try the control again.",
                );
                state.issue = Some(issue.clone());
                command_issue = Some(issue);
                box_.state = Some(state.clone());
            }
            if api.token.is_none() || (blocked && auxiliary_failure.is_none()) {
                let _ = shared.1.wait_timeout(box_, Duration::from_millis(100));
                continue;
            }
            if auxiliary_failure.is_none()
                && (box_
                    .front_owned_pause()
                    .is_some_and(|lane| lane.offers_native_pause(now))
                    || box_
                        .front_owned_previous()
                        .is_some_and(|lane| lane.offers_cued_previous(now))
                    || box_
                        .front_owned_next()
                        .is_some_and(|lane| lane.offers_native_next(now)))
            {
                // Give the coordinator one bounded chance to use the bound
                // native output before falling back to an addressed HTTP write.
                let _ = shared.1.wait_timeout(box_, Duration::from_millis(20));
                continue;
            }
            let refresh = std::mem::take(&mut box_.refresh);
            let control = if auxiliary_failure.is_none() && !box_.owned_http_waiting(now) {
                box_.controls.pop_front()
            } else {
                None
            };
            box_.resume_dispatch = control
                .as_ref()
                .and_then(|queued| ResumeDispatch::preparing(&queued.control));
            box_.http_mutating = control.is_some()
                || confirmation
                    .as_ref()
                    .is_some_and(ControlConfirmation::selects_output_or_context);
            let direct_owned_start = control
                .as_ref()
                .is_some_and(|queued| box_.prefers_direct_owned_start(&queued.control));
            box_.http_starting_targeted_item = direct_owned_start
                || control
                    .as_ref()
                    .is_some_and(|queued| starts_targeted_item(&queued.control))
                || confirmation.as_ref().is_some_and(|p| {
                    starts_targeted_item(&p.control) || box_.prefers_direct_owned_start(&p.control)
                });
            (
                control,
                direct_owned_start,
                box_.visible,
                refresh,
                auxiliary_failure,
            )
        };
        let mut failure = auxiliary_failure;
        let mut control_failed = false;
        let mut accepted_owned_relative = None;
        let mut operation_id = control
            .as_ref()
            .and_then(|queued| queued.control.operation_id());
        if let Some(QueuedControl {
            control,
            owned_lane,
            ..
        }) = control
        {
            let current_target = state
                .playback
                .as_ref()
                .and_then(|p| p.device.as_ref())
                .filter(|d| d.is_active && !d.is_restricted)
                .map(|d| d.id.as_str());
            let mut previous_intent = None;
            let scoped_target = owned_lane.and_then(|id| {
                let now = Instant::now();
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_.generation != generation || box_.stopping {
                    return None;
                }
                box_.recheck_owned_http(state.playback.as_ref(), now);
                let intent = box_.playback_intent_serial;
                let lane = box_
                    .owned_http
                    .as_mut()
                    .filter(|lane| lane.id == id && !lane.native_only() && lane.ready(now))?;
                if matches!(control, RemoteControl::Previous) && lane.waiting_to_send_previous() {
                    previous_intent = Some(intent);
                }
                lane.begin_http_followup();
                if owned_http::relative(&control) {
                    lane.begin_relative(now);
                }
                Some(lane.device.clone())
            });
            let target = if owned_lane.is_some() {
                scoped_target.as_deref()
            } else {
                current_target
            };
            let explicit_target = match &control {
                RemoteControl::StartOnDevice { device_id, .. }
                | RemoteControl::StartContext { device_id, .. }
                | RemoteControl::ResumeOnDevice { device_id, .. }
                | RemoteControl::PauseOnDevice { device_id }
                | RemoteControl::Transfer { device_id, .. } => Some(device_id.as_str()),
                _ => None,
            };
            let verified_target =
                explicit_target.map_or(target.is_some(), |device| target == Some(device));
            let trace_http = |stage| {
                if let Some(event) =
                    autoplay_trace::Event::transport(&control, stage, verified_target)
                {
                    event.write();
                }
            };
            let result = if owned_lane.is_some() && scoped_target.is_none() {
                Err(api::ApiFailure::invalid(owned_commands::unavailable()))
            } else {
                trace_http(TransportStage::HttpDispatch);
                api.control_with_owned_start(
                    &control,
                    target,
                    direct_owned_start,
                    operation_id.is_some(),
                )
            };
            if matches!(control, RemoteControl::ResumeOnDevice { .. }) && operation_id.is_some() {
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                let cancelled = box_.resume_dispatch.take().is_some_and(|resume| {
                    resume.matches_control(&control)
                        && resume.phase == ResumeDispatchPhase::Cancelled
                });
                if cancelled {
                    // A failed preflight can still report account-wide auth
                    // or quota state, but that is no longer this operation.
                    operation_id = None;
                }
                if cancelled
                    && !result
                        .as_ref()
                        .is_err_and(|failure| matches!(failure.status, 401 | 429))
                {
                    // The caller already received the definitive cancellation.
                    // No ordinary receipt, failure, or delayed Pause follows it.
                    box_.http_mutating = confirmation
                        .as_ref()
                        .is_some_and(ControlConfirmation::selects_output_or_context);
                    box_.refresh |= refresh;
                    continue;
                }
            }
            trace_http(match &result {
                Ok(_) => TransportStage::HttpAccepted,
                Err(error) => TransportStage::HttpFailed(error.status),
            });
            if let (Some(id), Some(intent), Err(error)) = (owned_lane, previous_intent, &result)
                && error.previous_response_timeout
            {
                let now = Instant::now();
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                // Ambiguity never authorizes a dependent queued write.
                box_.controls.retain(|queued| queued.owned_lane != Some(id));
                if box_.generation == generation
                    && box_.playback_intent_serial != intent
                    && !box_.controls.is_empty()
                {
                    // A newer independent intent survives this ambiguous old write.
                    if box_.owned_http.as_ref().is_some_and(|lane| lane.id == id) {
                        box_.cancel_owned_http();
                    }
                    confirmation = None;
                    box_.http_mutating = false;
                    next_poll = now;
                    continue;
                }
                if box_.generation == generation
                    && !box_.stopping
                    && !box_.authorization_revoked
                    && box_.rate(now).is_none()
                    && box_.auxiliary_failure.is_none()
                    && let Some(lane) = box_.owned_http.as_mut().filter(|lane| {
                        lane.id == id
                            && lane.can_reconcile_previous()
                            && !lane.conflicts_with(state.playback.as_ref())
                    })
                {
                    let grace = Duration::from_millis(1500);
                    lane.start_previous_observation(now, grace);
                    previous_observation = Some(PreviousObservation {
                        lane: id,
                        intent: box_.playback_intent_serial,
                        deadline: now + grace,
                        issue: error.error.clone(),
                    });
                    confirmation = None;
                    box_.http_mutating = false;
                    next_poll = now;
                    continue;
                }
            }
            if let Some(id) = owned_lane {
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_.generation == generation
                    && box_.owned_http.as_ref().is_some_and(|lane| lane.id == id)
                {
                    if result.is_err() {
                        box_.cancel_owned_http();
                    } else if owned_http::relative(&control)
                        && let Some(lane) = &mut box_.owned_http
                    {
                        lane.accepted(Instant::now());
                        accepted_owned_relative = Some(id);
                    }
                }
            }
            match result {
                Ok(acceptance) => {
                    if let Some(operation_id) = operation_id {
                        state.control_result = Some(RemoteControlResult {
                            operation_id,
                            error: None,
                        });
                    }
                    confirmation = Some(ControlConfirmation {
                        control: control.clone(),
                        previous: state.playback.as_ref().and_then(|frame| {
                            frame
                                .current
                                .as_ref()
                                .map(|item| (item.id.clone(), frame.position_ms))
                        }),
                        until: Instant::now() + Duration::from_secs(8),
                        attempts: 0,
                    });
                    next_poll = Instant::now();
                    state.issue = None;
                    command_issue = None;
                    if matches!(
                        control,
                        RemoteControl::Transfer {
                            operation_id: Some(_),
                            ..
                        }
                    ) {
                        let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        if box_.generation == generation {
                            box_.request_device_discovery(Instant::now(), true);
                            shared.1.notify_all();
                        }
                    }
                    if let Some(operation) = operation_id {
                        // Deliver the write receipt without waiting for a playback read.
                        // The unchanged observation still needs fresh playback evidence.
                        let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        if box_.generation == generation
                            && !box_.stopping
                            && !box_.authorization_revoked
                        {
                            box_.accepted_context_resume = match acceptance {
                                api::ControlAcceptance::ContextResume { dispatched_at } => {
                                    Some(output_clock::ContextResumeReceipt {
                                        operation,
                                        dispatched_at,
                                    })
                                }
                                api::ControlAcceptance::Standard => None,
                            };
                            box_.state = Some(state.clone());
                        }
                    }
                    if matches!(
                        control,
                        RemoteControl::Start { .. }
                            | RemoteControl::StartOnDevice { .. }
                            | RemoteControl::StartSpokenOnDevice { .. }
                            | RemoteControl::ResumeOnDevice { .. }
                            | RemoteControl::StartContext { .. }
                            | RemoteControl::Next
                            | RemoteControl::Previous
                            | RemoteControl::Transfer { .. }
                            | RemoteControl::Enqueue { .. }
                    ) {
                        next_queue = Instant::now();
                    }
                }
                Err(error) => {
                    if owned_http::replaces_lane(&control) {
                        let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        if box_.generation == generation {
                            box_.cancel_owned_http();
                        }
                    }
                    control_failed = true;
                    failure = Some(error);
                }
            }
        }
        if failure.is_none()
            && previous_observation.is_none()
            && (refresh || Instant::now() >= next_poll)
        {
            if let Some(id) = accepted_owned_relative {
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_
                    .front_owned_http()
                    .is_some_and(|lane| lane.id == id && lane.ready(Instant::now()))
                {
                    // The accepted write already has a fresh successor clock.
                    // Run the scoped follow-up before the due Web API read.
                    let trace = box_.controls.front().and_then(|entry| {
                        autoplay_trace::Event::transport(
                            &entry.control,
                            TransportStage::HttpReadDeferred,
                            true,
                        )
                    });
                    box_.record_trace(trace);
                    box_.refresh |= refresh;
                    continue;
                }
            }
            let pending_row_device = confirmation
                .as_ref()
                .and_then(ControlConfirmation::positioned_row_device);
            let owned_http = {
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if !read_deferred_for_row
                    && let Some(device) = pending_row_device
                    && box_.generation == generation
                    && !box_.stopping
                    && !box_.authorization_revoked
                    && box_.rate(now).is_none()
                    && box_.control_issue(now).is_none()
                    && box_.auxiliary_failure.is_none()
                    && box_.owned_http.is_none()
                    && let Some(next) = box_.controls.front().filter(|entry| {
                        entry.owned_lane.is_none()
                            && matches!(
                                &entry.control,
                                RemoteControl::StartContext {
                                    uri,
                                    device_id,
                                    item: Some(item),
                                    position: Some(position),
                                    query_id: Some(query_id),
                                    ..
                                } if device_id == device
                                    && box_.latest_query_id == Some(*query_id)
                                    && box_.context_page.as_ref().is_some_and(|page| {
                                        &page.context == uri && page.query_id == *query_id
                                            && page.rows.iter().any(|(row, id)| row == position && *id == item.id)
                                    })
                            )
                    })
                {
                    // A newer explicit row can use its own checked offset and device.
                    // Keep the write fence and defer at most one read per burst.
                    let verified_target = state
                        .playback
                        .as_ref()
                        .and_then(|frame| frame.device.as_ref())
                        .is_some_and(|active| {
                            active.id == device && active.is_active && !active.is_restricted
                        });
                    let trace = autoplay_trace::Event::transport(
                        &next.control,
                        TransportStage::HttpReadDeferred,
                        verified_target,
                    );
                    box_.record_trace(trace);
                    box_.refresh |= refresh;
                    read_deferred_for_row = true;
                    continue;
                }
                box_.owned_http
                    .as_ref()
                    .filter(|lane| {
                        lane.navigating()
                            || box_
                                .controls
                                .iter()
                                .any(|entry| entry.owned_lane == Some(lane.id))
                    })
                    .map(|lane| lane.id)
            };
            let pending_row = pending_row_device.is_some();
            let scoped = owned_http.is_some() || pending_row;
            let trace_read = autoplay_trace::enabled() && state.devices.is_empty();
            if trace_read
                && let Some(event) = autoplay_trace::Event::discovery(
                    autoplay_trace::DiscoveryStage::PlaybackReadDispatched { scoped },
                )
            {
                event.write();
            }
            read_deferred_for_row = false;
            let playback = if scoped {
                api.playback_during_owned_navigation()
            } else {
                api.playback()
            };
            if trace_read {
                let (status, has_frame) = match &playback {
                    Ok(Some(_)) => (200, true),
                    Ok(None) => (204, false),
                    Err(error) => (error.status, false),
                };
                if let Some(event) = autoplay_trace::Event::discovery(
                    autoplay_trace::DiscoveryStage::PlaybackReadReceived { status, has_frame },
                ) {
                    event.write();
                }
            }
            match playback {
                Ok(mut frame) => {
                    let same_binding = frame
                        .as_ref()
                        .zip(state.playback.as_ref())
                        .is_some_and(|(new, old)| QueueBinding::of(new) == QueueBinding::of(old));
                    if !same_binding {
                        // An in-flight reply can outlive A → B → A on this account.
                        queue_epoch = queue_epoch.wrapping_add(1);
                    }
                    if same_binding
                        && let (Some(new), Some(old)) = (frame.as_mut(), state.playback.as_ref())
                    {
                        new.upcoming = old.upcoming.clone();
                    }
                    observation_id = observation_id.wrapping_add(1);
                    state.observation_id = observation_id;
                    state.issue = frame
                        .as_ref()
                        .filter(|p| p.current.is_none())
                        .map(|_| {
                            PlayerError::new(
                                ErrorCode::TrackUnavailable,
                                "This content is not a supported music track.",
                            )
                        })
                        .or_else(|| command_issue.clone());
                    state.playback = frame;
                    state.connection = ProviderConnectionState::Connected;
                    state.retry_after_seconds = None;
                    state.authentication_required = false;
                    failures = 0;
                    next_poll = Instant::now() + Duration::from_secs(if visible { 2 } else { 15 });
                    if let Some(pending) = confirmation.as_mut() {
                        if pending.matches(state.playback.as_ref())
                            || Instant::now() >= pending.until
                        {
                            confirmation = None;
                        } else {
                            pending.attempts = pending.attempts.saturating_add(1);
                            next_poll = Instant::now()
                                + Duration::from_millis(if pending.attempts <= 2 {
                                    500
                                } else {
                                    1000
                                });
                        }
                    }
                    // Publish the transport before any catalog, discovery or queue IO.
                    publish(
                        &shared,
                        generation,
                        RemoteUpdate::State(Box::new(state.clone())),
                    );
                    let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    if box_.generation == generation && !box_.stopping {
                        box_.recheck_owned_http(state.playback.as_ref(), Instant::now());
                        box_.request_device_discovery(Instant::now(), false);
                        if let Some(frame) = &state.playback
                            && (refresh || !same_binding || Instant::now() >= next_queue)
                        {
                            let binding = QueueBinding::of(frame);
                            if binding.device.as_ref().is_some_and(|id| !id.is_empty())
                                && box_.local_queue_binding.as_ref() != Some(&binding)
                            {
                                box_.request_queue = Some(QueueRequest {
                                    binding,
                                    epoch: queue_epoch,
                                });
                            }
                            next_queue =
                                Instant::now() + Duration::from_secs(if visible { 15 } else { 60 });
                        }
                    }
                    drop(box_);
                    shared.1.notify_all();
                }
                Err(error)
                    if error.status == 0 && (owned_http.is_some() || pending_row) && {
                        let box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                        box_.generation == generation
                            && box_.owned_http.as_ref().is_some_and(|lane| {
                                owned_http == Some(lane.id)
                                    || pending_row && lane.awaiting_initial_row()
                            })
                    } =>
                {
                    // The already accepted mutation can be witnessed by a fresh local
                    // clock even when a cloud read fails. No mutation is replayed.
                    next_poll = Instant::now() + Duration::from_millis(500);
                }
                Err(error) => failure = Some(error),
            }
        }
        if let Some(error) = failure {
            let mut control_result =
                operation_id
                    .filter(|_| control_failed)
                    .map(|operation_id| RemoteControlResult {
                        operation_id,
                        error: Some(error.error.clone()),
                    });
            confirmation = None;
            state.issue = Some(error.error.clone());
            if error.status == 401 {
                api.token = None;
                state = disconnected(true);
                state.issue = Some(error.error);
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_.generation == generation {
                    box_.authorization_revoked = true;
                    box_.auxiliary_token = None;
                    box_.page = None;
                    box_.controls.clear();
                    box_.owned_http = None;
                    box_.query = None;
                    box_.details.clear();
                    box_.detail_credits.clear();
                    box_.detail_pages.clear();
                    box_.edit = None;
                    box_.edit_result = None;
                    box_.edit_busy = false;
                    box_.device_discovery = DeviceDiscovery::default();
                    box_.request_queue = None;
                    box_.devices = None;
                    box_.queue = None;
                }
            } else if control_failed && matches!(error.status, 400 | 403 | 404 | 409 | 499) {
                // A rejected operation does not turn a working output into a network outage.
                // The admitted operation is never retried; refresh its real capabilities.
                if operation_id.is_some() {
                    command_issue = None;
                    state.issue = None;
                } else {
                    command_issue = Some(error.error);
                }
                next_poll = Instant::now();
            } else {
                failures = failures.saturating_add(1);
                let delay = error
                    .retry_after
                    .unwrap_or_else(|| (1_u64 << failures.min(6)).min(60));
                if error.status == 429 {
                    let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    if box_.generation == generation
                        && !box_.stopping
                        && let Some(result) = box_.cancelled_control_result.take()
                    {
                        control_result = Some(result);
                    }
                    let limit = box_.rate(Instant::now());
                    state.retry_after_seconds = limit.map(|limit| limit.seconds);
                    if let Some(limit) = limit {
                        state.issue = Some(limit.issue());
                    }
                } else {
                    let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    if box_.generation == generation && !box_.stopping {
                        box_.control_backoff = Some(Cooldown::new(Instant::now(), delay));
                        box_.control_backoff_issue = Some(error.error.clone());
                        box_.cancel_controls(&error.error, Instant::now());
                        if let Some(result) = box_.cancelled_control_result.take() {
                            control_result = Some(result);
                        }
                    }
                    state.retry_after_seconds = Some(delay);
                }
                state.connection = ProviderConnectionState::Unavailable;
            }
            if let Some(result) = control_result {
                state.control_result = Some(result);
            }
            publish(
                &shared,
                generation,
                RemoteUpdate::State(Box::new(state.clone())),
            );
        }
        let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
        box_.http_mutating = confirmation
            .as_ref()
            .is_some_and(ControlConfirmation::selects_output_or_context);
        box_.http_starting_targeted_item = confirmation.as_ref().is_some_and(|p| {
            starts_targeted_item(&p.control) || box_.prefers_direct_owned_start(&p.control)
        });
        if !confirmation
            .as_ref()
            .is_some_and(|p| box_.prefers_direct_owned_start(&p.control))
            && !box_
                .controls
                .iter()
                .any(|queued| box_.prefers_direct_owned_start(&queued.control))
        {
            box_.direct_owned_start = None;
        }
        if box_.stopping {
            break;
        }
        if (box_.controls.is_empty() || box_.owned_http_waiting(Instant::now()))
            && box_.credential.is_none()
            && !box_.refresh
            && box_.devices.is_none()
            && box_.queue.is_none()
            && box_.auxiliary_failure.is_none()
        {
            let _ = shared.1.wait_timeout(box_, Duration::from_millis(100));
        }
    }
}

enum AuxiliaryWork {
    Edit(RemoteEditRequest),
    Catalog(RemoteQuery),
    Devices,
    Queue(QueueRequest),
    Autoplay(autoplay::Request),
}
fn run_auxiliary(shared: Shared, base: String) {
    let Ok(mut api) = Api::new(base, shared.clone()) else {
        return;
    };
    loop {
        let work = {
            let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
            if box_.stopping {
                break;
            }
            if box_.auxiliary_token.is_none() || box_.rate(Instant::now()).is_some() {
                let _ = shared.1.wait_timeout(box_, Duration::from_millis(100));
                continue;
            }
            api.token = box_.auxiliary_token.clone();
            api.generation = box_.generation;
            if let Some(edit) = box_.edit.take() {
                AuxiliaryWork::Edit(edit)
            } else if box_.device_discovery.start(Instant::now()) {
                AuxiliaryWork::Devices
            } else if let Some(query) = box_.query.take() {
                AuxiliaryWork::Catalog(query)
            } else if let Some(query) = box_.details.pop_front() {
                AuxiliaryWork::Catalog(query)
            } else if let Some(binding) = box_.request_queue.take() {
                AuxiliaryWork::Queue(binding)
            } else if let Some(request) = box_.autoplay_request.take() {
                AuxiliaryWork::Autoplay(request)
            } else {
                let wait =
                    box_.device_discovery
                        .retry_at
                        .map_or(Duration::from_millis(100), |due| {
                            due.saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(100))
                        });
                let _ = shared.1.wait_timeout(box_, wait);
                continue;
            }
        };
        let result = match work {
            AuxiliaryWork::Edit(request) => {
                let result = api.edit(&request.edit);
                let (collection, saved, error) = match &result {
                    Ok(content) => (content.collection.clone(), content.saved.clone(), None),
                    Err(failure) => (None, vec![], Some(failure.error.clone())),
                };
                // A 401 result must reach the host before authorization revocation drops account data.
                publish(
                    &shared,
                    api.generation,
                    RemoteUpdate::Edited(RemoteEditResult {
                        saved,
                        operation_id: request.operation_id,
                        collection,
                        error,
                    }),
                );
                result.map(|_| ())
            }

            AuxiliaryWork::Catalog(query) => {
                let result = api.catalog(&query);
                let page = match &result {
                    Ok(page) => page.clone(),
                    Err(error) => RemotePage {
                        position_edits: false,
                        related_collections: vec![],
                        detail: query.kind.is_detail(),
                        item_positions: vec![],
                        next_cursor: None,
                        provider: "spotify".into(),
                        query_id: query.query_id,
                        offset: query.offset,
                        total: 0,
                        items: vec![],
                        collections: vec![],
                        spoken_progress: vec![],
                        recent_plays: vec![],
                        issue: Some(error.error.clone()),
                    },
                };
                publish(&shared, api.generation, RemoteUpdate::Page(page));
                result.map(|_| ())
            }
            AuxiliaryWork::Devices => {
                if let Some(event) =
                    autoplay_trace::Event::discovery(autoplay_trace::DiscoveryStage::Dispatched)
                {
                    event.write();
                }
                let result = api.devices();
                let (status, count) = match &result {
                    Ok(devices) => (200, devices.len()),
                    Err(error) => (error.status, 0),
                };
                if let Some(event) =
                    autoplay_trace::Event::discovery(autoplay_trace::DiscoveryStage::Received {
                        status,
                        count,
                    })
                {
                    event.write();
                }
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_.generation != api.generation || box_.stopping || box_.authorization_revoked
                {
                    result.map(|_| ())
                } else {
                    let now = Instant::now();
                    let can_publish = box_.rate(now).is_none();
                    let empty = result
                        .as_ref()
                        .ok()
                        .filter(|_| can_publish)
                        .map(Vec::is_empty);
                    box_.device_discovery.finish(now, empty);
                    if let Some(due) = box_.device_discovery.retry_at {
                        box_.record_trace(autoplay_trace::Event::discovery(
                            autoplay_trace::DiscoveryStage::RetryScheduled {
                                delay_ms: due.saturating_duration_since(now).as_millis(),
                            },
                        ));
                    }
                    result.map(|devices| {
                        if can_publish {
                            box_.devices = Some(devices);
                        }
                    })
                }
            }
            AuxiliaryWork::Queue(request) => api.queue().map(|response| {
                if response.current != request.binding.media {
                    return;
                }
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_.generation == api.generation
                    && !box_.stopping
                    && !box_.authorization_revoked
                {
                    box_.queue = Some((request, response.upcoming));
                }
            }),
            AuxiliaryWork::Autoplay(request) => {
                let ticket = request.ticket();
                let (page, result) = match api.autoplay_page(request) {
                    Ok(page) => (Some(page), Ok(())),
                    Err(error) => (None, Err(error)),
                };
                let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if box_.generation == api.generation
                    && !box_.stopping
                    && !box_.authorization_revoked
                {
                    box_.autoplay_result = Some((ticket, page));
                }
                result
            }
        };
        if let Err(error) = result
            && matches!(error.status, 401 | 429)
        {
            let mut box_ = shared.0.lock().unwrap_or_else(|e| e.into_inner());
            if box_.generation == api.generation && !box_.stopping {
                if error.status == 401 {
                    box_.auxiliary_token = None;
                    box_.authorization_revoked = true;
                    box_.device_discovery = DeviceDiscovery::default();
                    box_.page = None;
                    box_.details.clear();
                    box_.detail_credits.clear();
                    box_.detail_pages.clear();
                }
                box_.auxiliary_failure = Some(error);
            }
        }
        shared.1.notify_all();
    }
}

fn validate_edit(request: &RemoteEditRequest) -> Result<(), PlayerError> {
    if request.operation_id == 0 {
        return Err(invalid("Invalid library operation"));
    }
    let valid_uris = |uris: &[String]| {
        !uris.is_empty() && uris.len() <= 100 && uris.iter().all(|uri| mapping::valid_uri(uri))
    };
    let valid_library_uris = |uris: &[String]| {
        (1..=40).contains(&uris.len()) && uris.iter().all(|uri| mapping::valid_library_uri(uri))
    };
    let valid_text = |name: &str, description: &str| {
        !name.trim().is_empty()
            && name.len() <= 256
            && description.len() <= 2048
            && !name.chars().any(char::is_control)
    };
    let valid = match &request.edit {
        RemoteEdit::CheckLibrary { uris } => valid_library_uris(uris),
        RemoteEdit::Library { uris, .. } => {
            valid_library_uris(uris) && uris.iter().all(|uri| !uri.starts_with("spotify:chapter:"))
        }
        RemoteEdit::PlaylistCover { id, jpeg_base64 } => {
            mapping::valid_id(id)
                && !jpeg_base64.is_empty()
                && jpeg_base64.len() <= 256 * 1024
                && jpeg_base64.len().is_multiple_of(4)
                && jpeg_base64
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
        }

        RemoteEdit::CreatePlaylist {
            name,
            description,
            public,
            collaborative,
        } => valid_text(name, description) && (!public || *collaborative != Some(true)),
        RemoteEdit::PlaylistDetails {
            id,
            name,
            description,
            public,
            collaborative,
        } => {
            mapping::valid_id(id)
                && (name.is_some()
                    || description.is_some()
                    || public.is_some()
                    || collaborative.is_some())
                && (*collaborative != Some(true) || *public == Some(false))
                && name.as_ref().is_none_or(|name| valid_text(name, ""))
                && description
                    .as_ref()
                    .is_none_or(|description| description.len() <= 2048)
        }
        RemoteEdit::PlaylistAdd { id, uris }
        | RemoteEdit::PlaylistRemove { id, uris }
        | RemoteEdit::PlaylistReplace { id, uris } => {
            mapping::valid_id(id)
                && (valid_uris(uris)
                    || matches!(&request.edit, RemoteEdit::PlaylistReplace { .. })
                        && uris.is_empty())
                && uris.iter().all(|uri| {
                    uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:")
                })
        }
        RemoteEdit::PlaylistMove {
            id,
            query_id,
            from,
            to,
        } => mapping::valid_id(id) && *query_id != 0 && *from < 100_000 && *to <= 100_000,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid("Invalid library change"))
    }
}
