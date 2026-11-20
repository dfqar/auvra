use player_domain::*;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const INTENT_LIFETIME: Duration = Duration::from_secs(2);
const NAVIGATION_PAUSE_LIMIT: Duration = Duration::from_millis(3500);
const PIPE_TIMEOUT: Duration = Duration::from_secs(3);
const START_TIMEOUT: Duration = Duration::from_secs(8);
const DISPATCH_WINDOW_MS: u64 = 1_500;
const TRANSPORT_SETTLE: Duration = Duration::from_millis(450);
const SOURCE_SWITCH_SETTLE: Duration = Duration::from_millis(DISPATCH_WINDOW_MS);
const SUCCESSOR_START_TOLERANCE_MS: u64 = 1_500;

#[derive(Default)]
pub(super) struct OwnedCommands {
    queued: VecDeque<Intent>,
    in_flight: Option<Sent>,
    accepted_seek: Option<Sent>,
    starting: Option<Sent>,
    transition: Option<TransportTransition>,
    confirmed_same_media_next: Option<SameMediaNext>,
    setting: Option<SettingDispatch>,
    serial: u64,
    failure: Option<(RemoteOutputControl, PlayerError)>,
}

struct Sent {
    command: RemoteOutputCommand,
    at: Instant,
    revoked: bool,
    seek: Option<SeekBarrier>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct SeekBarrier {
    queued_at: Instant,
    position_ms: u64,
    pub queue_epoch: Option<u64>,
}

struct Intent {
    control: RemoteOutputControl,
    request_id: Option<u64>,
    generation: u64,
    device: String,
    media: MediaId,
    queued_at: Instant,
    queue_epoch: Option<u64>,
    may_start_from_paused: bool,
    after_navigation: Option<u64>,
    navigation_pause: Option<(Instant, u64)>,
    after_play: Option<u64>,
    after_setting: Option<u64>,
    requires_control_clock: bool,
    targeted_context: Option<String>,
}

impl Intent {
    fn targeted_scope_matches(&self, clock: &RemoteOutputClock) -> bool {
        !self.requires_control_clock
            || self.generation == clock.generation
                && self.device == clock.device_id
                && Some(&self.media) == clock.media_id.as_ref()
                && self.queue_epoch == clock.queue_epoch
                && self.targeted_context.as_ref()
                    == clock
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.context_uri.as_ref())
    }

    fn matches_seek(&self, barrier: SeekBarrier) -> bool {
        self.queued_at == barrier.queued_at
            && self.queue_epoch == barrier.queue_epoch
            && self.control
                == (RemoteOutputControl::Seek {
                    position_ms: barrier.position_ms,
                })
    }

    fn seek_barrier(&self, clock: &RemoteOutputClock, now: Instant) -> Option<SeekBarrier> {
        let RemoteOutputControl::Seek { position_ms } = self.control else {
            return None;
        };
        (self.generation == clock.generation
            && self.device == clock.device_id
            && Some(&self.media) == clock.media_id.as_ref()
            && self.queue_epoch == clock.queue_epoch
            && now.saturating_duration_since(self.queued_at) < INTENT_LIFETIME)
            .then_some(SeekBarrier {
                queued_at: self.queued_at,
                position_ms,
                queue_epoch: self.queue_epoch,
            })
    }
}

impl SeekBarrier {
    pub(super) fn before_previous_restart(&self) -> bool {
        self.position_ms < 4_000
    }
}

impl Sent {
    fn is_previous_recovery(&self) -> bool {
        self.command.control == RemoteOutputControl::Play
            && self.command.required_queue_epoch.is_some()
    }

    fn seek_barrier(&self, clock: &RemoteOutputClock, now: Instant) -> Option<SeekBarrier> {
        self.seek.filter(|seek| {
            !self.revoked
                && self.command.generation == clock.generation
                && self.command.device_id == clock.device_id
                && Some(&self.command.media_id) == clock.media_id.as_ref()
                && seek.queue_epoch == clock.queue_epoch
                && now.saturating_duration_since(seek.queued_at) < INTENT_LIFETIME
        })
    }

    fn observed_seek(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        let Some(seek) = self.seek_barrier(clock, now) else {
            return false;
        };
        // The playing clock can advance after the seek. The paused sample
        // must be near the target itself, allowing for clock quantization.
        let advanced_ms = if clock.state == PlaybackState::Playing {
            (now.saturating_duration_since(self.at).as_secs_f64() * clock.rate * 1000.0) as u64
        } else {
            0
        };
        clock.sequence > self.command.sequence
            && super::owned_http::previous_prefers_web(clock)
            && clock.position_ms >= seek.position_ms.saturating_sub(120)
            && clock.position_ms <= seek.position_ms + advanced_ms + 120
    }
}

struct TransportTransition {
    command_id: u64,
    request_id: Option<u64>,
    acknowledged: bool,
    control: RemoteOutputControl,
    generation: u64,
    device: String,
    media: MediaId,
    sequence: u64,
    queue_epoch: Option<u64>,
    loading_followup: bool,
    at: Instant,
    source_switch_until: Option<Instant>,
    same_media_next: Option<SameMediaNext>,
}

struct SameMediaNext {
    request_id: u64,
    generation: u64,
    device: String,
    media: MediaId,
    queue_epoch: u64,
    sequence: u64,
    observed_at: Instant,
}

impl SameMediaNext {
    fn applies(&self, clock: Option<&RemoteOutputClock>, now: Instant) -> bool {
        now.saturating_duration_since(self.observed_at) < INTENT_LIFETIME
            && clock.is_some_and(|clock| {
                clock.active
                    && clock.generation == self.generation
                    && clock.device_id == self.device
                    && clock.media_id.as_ref() == Some(&self.media)
                    && clock.queue_epoch == Some(self.queue_epoch)
                    && clock.sequence >= self.sequence
            })
    }
}

struct SettingDispatch {
    command_id: u64,
    control: RemoteOutputControl,
    generation: u64,
    device: String,
    media: MediaId,
    queue_epoch: Option<u64>,
    sequence: u64,
    at: Instant,
    acknowledged: bool,
}

impl SettingDispatch {
    fn current_scope(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        clock.active
            && matches!(clock.state, PlaybackState::Paused | PlaybackState::Playing)
            && clock.generation == self.generation
            && clock.device_id == self.device
            && clock.media_id.as_ref() == Some(&self.media)
            && clock.queue_epoch == self.queue_epoch
            && now.saturating_duration_since(self.at) < INTENT_LIFETIME
    }
    fn matches_seek(&self, intent: &Intent) -> bool {
        matches!(intent.control, RemoteOutputControl::Seek { .. })
            && self.queue_epoch.is_some_and(|epoch| epoch > 0)
            && intent.generation == self.generation
            && intent.device == self.device
            && intent.media == self.media
            && intent.queue_epoch == self.queue_epoch
    }
}

impl TransportTransition {
    fn same_play_occurrence(&self, intent: &Intent, clock: &RemoteOutputClock) -> bool {
        self.acknowledged
            && self.control == RemoteOutputControl::Play
            && intent.generation == self.generation
            && intent.device == self.device
            && intent.media == self.media
            && intent.queue_epoch == self.queue_epoch
            && clock.active
            && clock.generation == self.generation
            && clock.device_id == self.device
            && clock.media_id.as_ref() == Some(&self.media)
            && clock.queue_epoch == self.queue_epoch
    }

    fn loading_followup_epoch(
        &self,
        intent: &Intent,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> Option<u64> {
        let epoch = self.queue_epoch.filter(|epoch| *epoch > 0)?;
        (matches!(
            intent.control,
            RemoteOutputControl::Next | RemoteOutputControl::Pause
        ) && now.saturating_duration_since(self.at) < INTENT_LIFETIME
            && self.same_play_occurrence(intent, clock)
            && clock.sequence > self.sequence
            && clock.state == PlaybackState::Loading
            && clock.rate == 0.0
            && supported(clock, &intent.control))
        .then_some(epoch)
    }

    fn rejected_loading_next(&self, intent: &Intent) -> bool {
        self.loading_followup
            && !self.acknowledged
            && self.control == RemoteOutputControl::Next
            && intent.control == RemoteOutputControl::Pause
            && intent.after_navigation == Some(self.command_id)
    }

    fn successor_pause_epoch(&self, clock: &RemoteOutputClock, now: Instant) -> Option<u64> {
        let old_epoch = self.queue_epoch?;
        let new_epoch = clock.queue_epoch.filter(|epoch| *epoch > old_epoch)?;
        let elapsed_ms = now.saturating_duration_since(self.at).as_millis() as u64;
        (clock.active
            && clock.generation == self.generation
            && clock.device_id == self.device
            && clock.sequence > self.sequence
            // An intermediate changed-item frame can carry the outgoing EOF
            // position. Wait for the successor's own near-start anchor.
            && clock.position_ms <= elapsed_ms.saturating_add(SUCCESSOR_START_TOLERANCE_MS)
            && clock
                .media_id
                .as_ref()
                .is_some_and(|media| !super::mapping::same_playback_id(media, &self.media))
            && (clock.state == PlaybackState::Paused && clock.rate == 0.0
                || clock.state == PlaybackState::Playing
                    && clock.rate > 0.0
                    && supported(clock, &RemoteOutputControl::Pause)))
        .then_some(new_epoch)
    }

    fn is_settled(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        if self.generation != clock.generation
            || self.device != clock.device_id
            || now.saturating_duration_since(self.at) >= TRANSPORT_SETTLE
        {
            return true;
        }
        clock.sequence > self.sequence
            && (clock.media_id.as_ref() != Some(&self.media)
                || (is_navigation(&self.control) && clock.queue_epoch != self.queue_epoch)
                || playback_satisfied(clock, &self.control))
    }
}

pub(super) fn busy() -> PlayerError {
    PlayerError::new(
        ErrorCode::RateLimited,
        "Wait for the current playback command to finish.",
    )
}

pub(super) fn unavailable() -> PlayerError {
    PlayerError::new(
        ErrorCode::ProviderUnavailable,
        "The local playback state changed. Try again.",
    )
}

pub(super) fn convert(
    control: &RemoteControl,
    clock: Option<&RemoteOutputClock>,
) -> Option<RemoteOutputControl> {
    use RemoteOutputControl as Local;
    Some(match control {
        RemoteControl::Play => Local::Play,
        RemoteControl::StartOnDevice {
            item,
            device_id,
            operation_id: Some(operation_id),
        } if clock.is_some_and(|c| *device_id == c.device_id) => {
            let uri = super::mapping::track_uri(item).ok()?;
            if !local_start_uri(&uri) {
                return None;
            }
            Local::StartUri {
                uri,
                operation_id: *operation_id,
            }
        }
        RemoteControl::StartContext {
            uri,
            device_id,
            operation_id: Some(operation_id),
            item: None,
            position: None,
            query_id: None,
        } if clock.is_some_and(|c| *device_id == c.device_id) && local_start_uri(uri) => {
            Local::StartUri {
                uri: uri.clone(),
                operation_id: *operation_id,
            }
        }
        RemoteControl::Pause => Local::Pause,
        RemoteControl::PauseOnDevice { device_id }
            if clock.is_some_and(|clock| {
                clock.device_id == *device_id
                    && clock.active
                    && clock.state == PlaybackState::Playing
                    && clock.rate > 0.0
                    && clock.queue_epoch.is_some_and(|epoch| epoch > 0)
                    && clock.actions.as_ref().is_some_and(|actions| actions.pause)
            }) =>
        {
            // The caller supplies only OutputProjection::control_clock here.
            // A row witness or a foreign destination keeps its addressed HTTP route.
            Local::Pause
        }
        RemoteControl::Seek { position_ms } => Local::Seek {
            position_ms: *position_ms,
        },
        RemoteControl::Next => Local::Next,
        RemoteControl::Previous => Local::Previous,
        RemoteControl::Volume { volume } => Local::Volume {
            percent: (volume * 100.0).round() as u8,
        },
        RemoteControl::Shuffle { enabled } => Local::Shuffle { enabled: *enabled },
        RemoteControl::Repeat { mode } => Local::Repeat { mode: *mode },
        RemoteControl::Enqueue { item } if item.id.as_str().starts_with("spotify:track:") => {
            Local::Enqueue {
                uri: item.id.as_str().into(),
            }
        }
        _ => return None,
    })
}

fn local_start_uri(uri: &str) -> bool {
    [
        "spotify:track:",
        "spotify:episode:",
        "spotify:album:",
        "spotify:playlist:",
    ]
    .iter()
    .any(|prefix| {
        uri.strip_prefix(prefix)
            .is_some_and(super::mapping::valid_id)
    })
}

fn supported(clock: &RemoteOutputClock, control: &RemoteOutputControl) -> bool {
    use RemoteOutputControl::*;
    if matches!(control, Volume { .. }) {
        return clock.volume.is_some();
    }
    if matches!(control, StartUri { .. }) {
        return true;
    }
    let Some(actions) = &clock.actions else {
        return false;
    };
    match control {
        Play => actions.play,
        Pause => actions.pause,
        Seek { position_ms } => actions.seek && *position_ms <= 86_400_000,
        Next => actions.next,
        Previous => actions.previous,
        PreviousFromStart => actions.previous && actions.seek,
        Shuffle { .. } => actions.shuffle,
        Repeat { .. } => actions.repeat,
        Enqueue { .. } => actions.queue_add,
        AutoplayEnqueue { .. } => {
            actions.queue_add
                && clock.state == PlaybackState::Playing
                && clock
                    .options
                    .as_ref()
                    .is_some_and(|options| options.repeat == RepeatMode::Off)
        }
        Volume { .. } | StartUri { .. } => unreachable!(),
    }
}

fn is_navigation(control: &RemoteOutputControl) -> bool {
    matches!(
        control,
        RemoteOutputControl::Next
            | RemoteOutputControl::Previous
            | RemoteOutputControl::PreviousFromStart
    )
}

fn is_playback(control: &RemoteOutputControl) -> bool {
    matches!(
        control,
        RemoteOutputControl::Play | RemoteOutputControl::Pause
    )
}

fn is_output_transport(control: &RemoteOutputControl) -> bool {
    is_navigation(control) || is_playback(control)
}

fn is_setting(control: &RemoteOutputControl) -> bool {
    matches!(
        control,
        RemoteOutputControl::Repeat { .. } | RemoteOutputControl::Shuffle { .. }
    )
}

fn starts_from_paused(control: &RemoteOutputControl, clock: &RemoteOutputClock) -> bool {
    clock.state == PlaybackState::Paused
        && (is_navigation(control) || matches!(control, RemoteOutputControl::Play))
}

fn playback_satisfied(clock: &RemoteOutputClock, control: &RemoteOutputControl) -> bool {
    matches!(control, RemoteOutputControl::Pause) && clock.state == PlaybackState::Paused
        || matches!(control, RemoteOutputControl::Play)
            && clock.state == PlaybackState::Playing
            && clock.rate > 0.0
}

fn supersedes(new: &RemoteOutputControl, old: &RemoteOutputControl) -> bool {
    use RemoteOutputControl::*;
    matches!(
        (new, old),
        (Play | Pause, Play | Pause)
            | (
                StartUri { .. },
                StartUri { .. } | Play | Pause | Seek { .. } | Next | Previous | PreviousFromStart
            )
            | (Pause, StartUri { .. })
            | (Seek { .. }, Seek { .. })
            | (Volume { .. }, Volume { .. })
            | (Repeat { .. }, Repeat { .. })
            | (Shuffle { .. }, Shuffle { .. })
    )
}

impl OwnedCommands {
    pub fn idle_for_autoplay(&self) -> bool {
        self.queued.is_empty() && self.in_flight.is_none() && !self.has_pending_start()
    }

    pub fn cancel_autoplay(&mut self) {
        self.queued.retain(|intent| {
            !matches!(intent.control, RemoteOutputControl::AutoplayEnqueue { .. })
        });
        if let Some(sent) = &mut self.in_flight
            && matches!(
                sent.command.control,
                RemoteOutputControl::AutoplayEnqueue { .. }
            )
        {
            sent.revoked = true;
        }
    }
    pub fn revoke(&mut self) {
        self.queued.clear();
        self.accepted_seek = None;
        self.failure = None;
        self.transition = None;
        self.confirmed_same_media_next = None;
        self.setting = None;
        // A sent request retains its credit until receipt/timeout. It might
        // already have reached the output when the binding was revoked.
        if let Some(sent) = &mut self.in_flight {
            sent.revoked = true;
        }
    }
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
    pub(super) fn awaiting_scoped_control(
        &self,
        id: u64,
        generation: u64,
    ) -> Option<&RemoteOutputControl> {
        self.in_flight
            .as_ref()
            .filter(|sent| {
                !sent.revoked
                    && sent.command.required_queue_epoch.is_some()
                    && sent.command.id == id
                    && sent.command.generation == generation
            })
            .map(|sent| &sent.command.control)
    }
    pub(super) fn dispatch_exact_pause(
        &mut self,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> Option<RemoteOutputCommand> {
        if clock.state != PlaybackState::Playing || clock.rate <= 0.0 {
            return None;
        }
        self.dispatch_scoped(RemoteOutputControl::Pause, clock, now, None)
    }
    pub(super) fn dispatch_previous_from_start(
        &mut self,
        clock: &RemoteOutputClock,
        now: Instant,
        request_id: Option<u64>,
    ) -> Option<RemoteOutputCommand> {
        if !super::owned_http::previous_prefers_web(clock) {
            return None;
        }
        self.dispatch_scoped(
            RemoteOutputControl::PreviousFromStart,
            clock,
            now,
            request_id,
        )
    }
    pub(super) fn dispatch_scoped(
        &mut self,
        control: RemoteOutputControl,
        clock: &RemoteOutputClock,
        now: Instant,
        request_id: Option<u64>,
    ) -> Option<RemoteOutputCommand> {
        if !self.scoped_dispatch_ready(&control, clock) {
            return None;
        }
        self.push_with_request_id(control, clock, now, request_id)
            .ok()?;
        let Some(RemoteUpdate::OutputCommand(mut command)) = self.poll(Some(clock), now) else {
            self.queued.clear();
            return None;
        };
        command.required_queue_epoch = clock.queue_epoch;
        self.in_flight.as_mut()?.command.required_queue_epoch = clock.queue_epoch;
        Some(command)
    }
    pub(super) fn scoped_dispatch_ready(
        &self,
        control: &RemoteOutputControl,
        clock: &RemoteOutputClock,
    ) -> bool {
        self.idle_for_autoplay()
            && self.failure.is_none()
            && clock.active
            && clock.queue_epoch.is_some_and(|epoch| epoch > 0)
            && supported(clock, control)
    }
    pub(super) fn seek_before_previous(
        &self,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> Option<SeekBarrier> {
        let sent = self.in_flight.as_ref();
        if self.queued.len() > 1 || sent.is_some_and(|sent| sent.seek_barrier(clock, now).is_none())
        {
            return None;
        }
        if let Some(intent) = self.queued.front() {
            return intent.seek_barrier(clock, now);
        }
        sent.or(self.accepted_seek.as_ref())?
            .seek_barrier(clock, now)
    }
    pub(super) fn waits_for_seek(&self, barrier: SeekBarrier, now: Instant) -> bool {
        now.saturating_duration_since(barrier.queued_at) < INTENT_LIFETIME
            && (self.queued_seek(barrier)
                || self
                    .in_flight
                    .as_ref()
                    .is_some_and(|sent| !sent.revoked && sent.seek == Some(barrier))
                || self
                    .accepted_seek
                    .as_ref()
                    .is_some_and(|sent| sent.seek == Some(barrier)))
    }
    pub(super) fn queued_seek(&self, barrier: SeekBarrier) -> bool {
        self.queued
            .front()
            .is_some_and(|intent| intent.matches_seek(barrier))
    }
    pub(super) fn seek_confirmed(
        &self,
        barrier: SeekBarrier,
        clock: Option<&RemoteOutputClock>,
        now: Instant,
    ) -> bool {
        self.accepted_seek.as_ref().is_some_and(|sent| {
            sent.seek == Some(barrier) && clock.is_some_and(|clock| sent.observed_seek(clock, now))
        })
    }
    pub(super) fn revoke_output_transport_except(&mut self, retained: Option<(u64, u64)>) {
        self.accepted_seek = None;
        self.transition = None;
        self.confirmed_same_media_next = None;
        self.setting = None;
        let revoked =
            |control: &RemoteOutputControl| is_output_transport(control) || is_setting(control);
        self.queued.retain(|intent| !revoked(&intent.control));
        if let Some(sent) = &mut self.in_flight
            && revoked(&sent.command.control)
            && !(sent.command.control == RemoteOutputControl::Pause
                && Some((sent.command.id, sent.command.generation)) == retained)
        {
            sent.revoked = true;
        }
    }
    #[cfg(test)]
    pub fn revoke_output_transport(&mut self) {
        self.revoke_output_transport_except(None);
    }
    pub(super) fn revoke_resume_pause(&mut self, id: u64, generation: u64) {
        if let Some(sent) = &mut self.in_flight
            && sent.command.control == RemoteOutputControl::Pause
            && sent.command.id == id
            && sent.command.generation == generation
        {
            sent.revoked = true;
        }
    }
    pub fn waits_for_setting_actions(
        &self,
        control: &RemoteOutputControl,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> bool {
        clock.actions.is_none()
            && self.setting.as_ref().is_some_and(|sent| {
                clock.sequence > sent.sequence
                    && sent.current_scope(clock, now)
                    && (is_setting(control) && supersedes(control, &sent.control)
                        || matches!(control, RemoteOutputControl::Seek { .. })
                            && sent.acknowledged
                            && sent.queue_epoch.is_some_and(|epoch| epoch > 0))
            })
    }
    pub fn next_is_output_transport(&self) -> bool {
        self.queued
            .front()
            .is_some_and(|intent| is_output_transport(&intent.control))
    }
    pub(super) fn next_requires_control_clock(&self) -> bool {
        self.queued
            .front()
            .is_some_and(|intent| intent.requires_control_clock)
    }
    pub(super) fn targeted_pause_can_dispatch(&self, clock: &RemoteOutputClock) -> bool {
        self.queued.front().is_none_or(|intent| {
            !intent.requires_control_clock
                || intent.targeted_scope_matches(clock)
                    && clock.state == PlaybackState::Playing
                    && clock.rate > 0.0
                    && supported(clock, &RemoteOutputControl::Pause)
        })
    }
    pub fn owned_next_ready_for_request(&self, request_id: u64) -> bool {
        self.front_request_ready(request_id, |control| *control == RemoteOutputControl::Next)
    }
    pub fn owned_seek_ready_for_request(&self, request_id: u64) -> bool {
        self.front_request_ready(request_id, |control| {
            matches!(control, RemoteOutputControl::Seek { .. })
        })
    }
    pub fn owned_play_pause_ready_for_request(&self, request_id: u64) -> bool {
        self.front_request_ready(request_id, is_playback)
    }
    fn front_request_ready(
        &self,
        request_id: u64,
        matches_control: impl FnOnce(&RemoteOutputControl) -> bool,
    ) -> bool {
        self.in_flight.is_none()
            && self.queued.front().is_some_and(|intent| {
                intent.request_id == Some(request_id) && matches_control(&intent.control)
            })
    }
    pub(super) fn owned_followup_ready(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        self.in_flight.is_none()
            && self.failure.is_none()
            && self
                .queued
                .front()
                .zip(self.transition.as_ref())
                .is_some_and(|(intent, transition)| {
                    now.saturating_duration_since(intent.queued_at) < INTENT_LIFETIME
                        && (transition
                            .loading_followup_epoch(intent, clock, now)
                            .is_some()
                            || transition.rejected_loading_next(intent)
                                && transition.successor_pause_epoch(clock, now).is_some())
                })
    }
    pub fn has_pending_output_transport(&self, now: Instant) -> bool {
        self.transition.as_ref().is_some_and(|transition| {
            now.saturating_duration_since(transition.at) < TRANSPORT_SETTLE
        }) || self
            .in_flight
            .as_ref()
            .is_some_and(|sent| !sent.revoked && is_output_transport(&sent.command.control))
            || self
                .queued
                .iter()
                .any(|intent| is_output_transport(&intent.control))
    }
    pub fn has_pending_next(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        if let Some(intent) = self
            .queued
            .iter()
            .rfind(|intent| is_navigation(&intent.control))
        {
            return intent.control == RemoteOutputControl::Next
                && intent.generation == clock.generation
                && intent.device == clock.device_id
                && now.saturating_duration_since(intent.queued_at) < INTENT_LIFETIME;
        }
        self.transition.as_ref().is_some_and(|transition| {
            transition.control == RemoteOutputControl::Next
                && transition.generation == clock.generation
                && transition.device == clock.device_id
                && now.saturating_duration_since(transition.at) < TRANSPORT_SETTLE
                && (transition.acknowledged
                    || self.in_flight.as_ref().is_some_and(|sent| {
                        !sent.revoked && sent.command.id == transition.command_id
                    }))
        })
    }
    pub fn has_pending_start(&self) -> bool {
        let pending = |sent: &Sent| {
            sent.command.control.operation_id().is_some() && sent.at.elapsed() < START_TIMEOUT
        };
        self.starting.as_ref().is_some_and(pending)
            || self.in_flight.as_ref().is_some_and(pending)
            || self
                .queued
                .iter()
                .any(|intent| intent.control.operation_id().is_some())
    }
    pub fn has_pending_silent_activation(&self, now: Instant) -> bool {
        self.queued.iter().any(|intent| {
            intent.may_start_from_paused
                && now.saturating_duration_since(intent.queued_at) < INTENT_LIFETIME
        }) || self.transition.as_ref().is_some_and(|transition| {
            transition
                .source_switch_until
                .is_some_and(|until| now < until)
        })
    }
    pub fn observe_projected_playback(&mut self, clock: Option<&RemoteOutputClock>, now: Instant) {
        if let Some(transition) = &mut self.transition
            && transition.source_switch_until.is_some()
            && clock.is_some_and(|clock| {
                clock.generation == transition.generation
                    && clock.device_id == transition.device
                    && clock.sequence > transition.sequence
                    && clock.state == PlaybackState::Playing
                    && clock.rate > 0.0
            })
        {
            transition.source_switch_until = None;
        }
        let Some(sent) = &self.starting else { return };
        let RemoteOutputControl::StartUri { uri, .. } = &sent.command.control else {
            return;
        };
        let elapsed = now.saturating_duration_since(sent.at);
        let confirmed = clock.is_some_and(|clock| {
            clock.generation == sent.command.generation
                && clock.device_id == sent.command.device_id
                && clock.sequence > sent.command.sequence
                && clock.state == PlaybackState::Playing
                && clock.position_ms <= elapsed.as_millis() as u64 + 4000
                && (clock.media_id.as_ref().is_some_and(|id| id.as_str() == uri)
                    || clock.metadata.as_ref().and_then(|m| m.context_uri.as_ref()) == Some(uri))
        });
        if confirmed || elapsed >= START_TIMEOUT {
            self.starting = None;
        }
    }

    pub fn has_same_media_next_observation(&self) -> bool {
        self.transition
            .as_ref()
            .is_some_and(|t| t.same_media_next.is_some())
    }

    pub fn project_same_media_next(
        &mut self,
        clock: Option<&RemoteOutputClock>,
        now: Instant,
    ) -> Option<u64> {
        if let Some(transition) = &mut self.transition {
            if transition
                .same_media_next
                .as_ref()
                .is_some_and(|proof| !proof.applies(clock, now))
            {
                transition.same_media_next = None;
            }
            if transition.same_media_next.is_none()
                && let (Some(request_id), Some(clock), Some(old_epoch)) =
                    (transition.request_id, clock, transition.queue_epoch)
                && transition.control == RemoteOutputControl::Next
                && now.saturating_duration_since(transition.at) < INTENT_LIFETIME
                && clock.confirmed_same_media_next == Some(transition.command_id)
                && clock.generation == transition.generation
                && clock.device_id == transition.device
                && clock.media_id.as_ref() == Some(&transition.media)
                && clock.sequence > transition.sequence
                && clock.active
                && clock.state == PlaybackState::Playing
                && clock.rate > 0.0
                && clock.actions.is_some()
                && let Some(queue_epoch) = clock.queue_epoch.filter(|epoch| *epoch > old_epoch)
            {
                transition.same_media_next = Some(SameMediaNext {
                    request_id,
                    generation: clock.generation,
                    device: transition.device.clone(),
                    media: transition.media.clone(),
                    queue_epoch,
                    sequence: clock.sequence,
                    observed_at: now,
                });
            }
            if transition.acknowledged
                && let Some(proof) = transition.same_media_next.take()
            {
                transition.request_id = None;
                self.confirmed_same_media_next = Some(proof);
            }
        }
        if self
            .confirmed_same_media_next
            .as_ref()
            .is_some_and(|proof| !proof.applies(clock, now))
        {
            self.confirmed_same_media_next = None;
        }
        self.confirmed_same_media_next
            .as_ref()
            .map(|proof| proof.request_id)
    }
    pub fn push(
        &mut self,
        control: RemoteOutputControl,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> Result<(), PlayerError> {
        self.push_with_request_id(control, clock, now, None)
    }

    pub fn push_with_request_id(
        &mut self,
        control: RemoteOutputControl,
        clock: &RemoteOutputClock,
        now: Instant,
        request_id: Option<u64>,
    ) -> Result<(), PlayerError> {
        let media = clock.media_id.clone().ok_or_else(unavailable)?;
        if !is_output_transport(&control)
            && !supported(clock, &control)
            && !self.waits_for_setting_actions(&control, clock, now)
        {
            return Err(unavailable());
        }
        self.queued
            .retain(|old| !supersedes(&control, &old.control));
        if self.queued.len() >= 8 {
            return Err(busy());
        }
        self.accepted_seek = None;
        let after_navigation = if control == RemoteOutputControl::Pause
            && self
                .queued
                .iter()
                .all(|intent| matches!(intent.control, RemoteOutputControl::Volume { .. }))
        {
            self.transition
                .as_ref()
                .filter(|transition| {
                    is_navigation(&transition.control)
                        && transition.generation == clock.generation
                        && transition.device == clock.device_id
                        && transition.media == media
                        && (now.saturating_duration_since(transition.at) < TRANSPORT_SETTLE
                            || clock.state == PlaybackState::Loading
                            || self.in_flight.as_ref().is_some_and(|sent| {
                                !sent.revoked && sent.command.id == transition.command_id
                            }))
                })
                .map(|transition| transition.command_id)
        } else {
            None
        };
        let after_setting = self
            .setting
            .as_ref()
            .filter(|sent| {
                matches!(control, RemoteOutputControl::Seek { .. })
                    && sent.acknowledged
                    && sent.queue_epoch.is_some_and(|epoch| epoch > 0)
                    && sent.current_scope(clock, now)
                    && (clock.actions.is_none() || clock.sequence <= sent.sequence)
            })
            .map(|sent| sent.command_id);
        self.queued.push_back(Intent {
            may_start_from_paused: starts_from_paused(&control, clock),
            request_id: request_id.filter(|id| {
                *id > 0
                    && (is_playback(&control)
                        || matches!(
                            control,
                            RemoteOutputControl::Next
                                | RemoteOutputControl::Previous
                                | RemoteOutputControl::PreviousFromStart
                                | RemoteOutputControl::Seek { .. }
                        ))
            }),
            control,
            generation: clock.generation,
            device: clock.device_id.clone(),
            media,
            queued_at: now,
            queue_epoch: clock.queue_epoch,
            after_navigation,
            navigation_pause: None,
            after_play: None,
            after_setting,
            requires_control_clock: false,
            targeted_context: None,
        });
        Ok(())
    }
    pub(super) fn push_targeted_pause(
        &mut self,
        clock: &RemoteOutputClock,
        now: Instant,
        request_id: Option<u64>,
    ) -> Result<(), PlayerError> {
        self.push_with_request_id(RemoteOutputControl::Pause, clock, now, request_id)?;
        let intent = self
            .queued
            .back_mut()
            .expect("successful push retains the new intent");
        intent.requires_control_clock = true;
        intent.targeted_context = clock
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.context_uri.clone());
        Ok(())
    }
    pub fn acknowledge(
        &mut self,
        result: RemoteOutputCommandResult,
    ) -> Option<RemoteOutputControl> {
        if !self.in_flight.as_ref().is_some_and(|sent| {
            sent.command.id == result.id && sent.command.generation == result.generation
        }) {
            return None;
        }
        let sent = self.in_flight.take()?;
        if sent.revoked {
            return None;
        }
        if let Some(transition) = &mut self.transition
            && transition.command_id == sent.command.id
        {
            transition.acknowledged = result.accepted;
            if !result.accepted {
                transition.request_id = None;
                transition.same_media_next = None;
            }
            if transition.source_switch_until.is_some() {
                // The helper's reply may arrive before the new playback state.
                transition.source_switch_until = Some(Instant::now() + SOURCE_SWITCH_SETTLE);
            }
        }
        if is_setting(&sent.command.control) {
            if result.accepted
                && let Some(setting) = &mut self.setting
                && setting.command_id == sent.command.id
            {
                setting.acknowledged = true;
                for intent in &mut self.queued {
                    if is_output_transport(&intent.control) {
                        break;
                    }
                    if setting.matches_seek(intent) {
                        intent.after_setting = Some(setting.command_id);
                    }
                }
            } else if !result.accepted {
                self.setting = None;
            }
        }
        if result.accepted && sent.command.control == RemoteOutputControl::Play {
            // Only seeks already waiting behind this Play need the resulting
            // clock. Ordinary seeks on stable paused playback stay immediate.
            for intent in &mut self.queued {
                if is_output_transport(&intent.control) {
                    break;
                }
                if matches!(intent.control, RemoteOutputControl::Seek { .. })
                    && intent.generation == sent.command.generation
                    && intent.device == sent.command.device_id
                    && intent.media == sent.command.media_id
                {
                    intent.after_play = Some(sent.command.sequence);
                }
            }
        }
        if result.accepted && sent.seek.is_some() {
            let control = sent.command.control.clone();
            self.accepted_seek = Some(sent);
            return Some(control);
        }
        if !result.accepted && sent.is_previous_recovery() {
            // This optional correction may be vetoed by a newer host intent.
            // It has no fallback and cannot consume later explicit commands.
            return None;
        }
        let control = self.track_dispatched_start(sent);
        if result.accepted {
            return Some(control);
        }
        if control == RemoteOutputControl::Next
            && let Some(transition) = self.transition.as_ref().filter(|transition| {
                transition.command_id == result.id && transition.loading_followup
            })
            && let Some(pause) = self
                .queued
                .iter_mut()
                .find(|intent| !matches!(intent.control, RemoteOutputControl::Volume { .. }))
            && pause.control == RemoteOutputControl::Pause
            && pause.generation == result.generation
            && pause.device == transition.device
        {
            // EOF can advance the occurrence before the scoped Next is written.
            // Keep the trailing Pause, but never send that Next on the successor.
            pause.after_navigation = Some(result.id);
            self.queued.retain(|intent| {
                intent.control.operation_id().is_some()
                    || intent.after_navigation == Some(result.id)
                        && intent.control == RemoteOutputControl::Pause
            });
            return None;
        }
        if matches!(control, RemoteOutputControl::AutoplayEnqueue { .. }) {
            return None;
        }
        if is_playback(&control)
            && self
                .queued
                .iter()
                .any(|intent| is_playback(&intent.control))
        {
            // The newer Play/Pause is the current user intent.
            return None;
        }
        self.queued
            .retain(|intent| intent.control.operation_id().is_some());
        self.failure = Some((
            control,
            PlayerError::new(
                ErrorCode::ProviderUnavailable,
                "The local playback engine could not confirm the command. Check playback before trying again.",
            ),
        ));
        None
    }
    fn track_dispatched_start(&mut self, sent: Sent) -> RemoteOutputControl {
        if sent.command.control.operation_id().is_some() {
            // A timeout or a lost reply cannot prove that playback did not start.
            let control = sent.command.control.clone();
            self.starting = Some(sent);
            control
        } else {
            sent.command.control
        }
    }
    pub fn poll(
        &mut self,
        clock: Option<&RemoteOutputClock>,
        now: Instant,
    ) -> Option<RemoteUpdate> {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|sent| now.saturating_duration_since(sent.at) >= PIPE_TIMEOUT)
        {
            let sent = self.in_flight.take()?;
            if is_setting(&sent.command.control) {
                self.setting = None;
            }
            if !sent.is_previous_recovery()
                && !matches!(
                    sent.command.control,
                    RemoteOutputControl::AutoplayEnqueue { .. }
                )
            {
                self.queued
                    .retain(|intent| intent.control.operation_id().is_some());
                if !sent.revoked {
                    let control = self.track_dispatched_start(sent);
                    self.failure = Some((
                        control,
                        PlayerError::new(
                            ErrorCode::ProviderUnavailable,
                            "The local playback command timed out. Check playback before trying again.",
                        ),
                    ));
                }
            }
        }
        if let Some((control, error)) = self.failure.take() {
            return Some(RemoteUpdate::OutputCommandFailed { control, error });
        }
        if self.in_flight.is_some() {
            return None;
        }
        let intent = self.queued.front_mut()?;
        let output_transport = is_output_transport(&intent.control);
        let ordinarily_fresh = now.saturating_duration_since(intent.queued_at) < INTENT_LIFETIME;
        let navigation = self.transition.as_ref().filter(|transition| {
            intent.after_navigation == Some(transition.command_id) && transition.acknowledged
        });
        if ordinarily_fresh
            && let Some(transition) = navigation
            && let Some(clock) = clock
            && clock.active
            && clock.generation == transition.generation
            && clock.device_id == transition.device
            && clock.sequence > transition.sequence
            && clock.state == PlaybackState::Loading
            && clock.actions.is_none()
        {
            // A newer actionless clock on this acknowledged navigation can
            // prolong the trailing Pause, measured from the actual send.
            intent.navigation_pause =
                Some((transition.at + NAVIGATION_PAUSE_LIMIT, clock.sequence));
        }
        let extended_navigation = navigation.zip(
            intent
                .navigation_pause
                .filter(|(deadline, _)| now < *deadline)
                .map(|(_, loading_sequence)| loading_sequence),
        );
        let fresh = ordinarily_fresh || extended_navigation.is_some();
        if fresh && clock.is_none() && (output_transport || intent.control.operation_id().is_some())
        {
            return None;
        }
        if ordinarily_fresh
            && intent.control == RemoteOutputControl::Pause
            && self
                .transition
                .as_ref()
                .zip(clock)
                .is_some_and(|(play, clock)| {
                    play.same_play_occurrence(intent, clock) && clock.state == PlaybackState::Paused
                })
        {
            // The clock that preceded the accepted Play cannot confirm its
            // trailing Pause, even after the ordinary settle timer expires.
            return None;
        }
        if ordinarily_fresh
            && let Some(sequence) = intent.after_play
            && self.transition.as_ref().is_some_and(|transition| {
                transition.control == RemoteOutputControl::Play
                    && transition.sequence == sequence
                    && transition.generation == intent.generation
                    && transition.device == intent.device
                    && transition.media == intent.media
            })
            && clock.is_none_or(|clock| {
                clock.generation == intent.generation
                    && clock.device_id == intent.device
                    && clock.media_id.as_ref() == Some(&intent.media)
                    && (clock.sequence <= sequence
                        || !playback_satisfied(clock, &RemoteOutputControl::Play)
                        || clock.actions.is_none())
            })
        {
            // Play's receipt can precede its same-item clock or the next action
            // snapshot. Keep the seek queued until the host can send it once.
            return None;
        }
        let after_setting = self.setting.as_ref().filter(|setting| {
            intent.after_setting == Some(setting.command_id) && setting.acknowledged
        });
        if ordinarily_fresh
            && let Some(setting) = after_setting
            && clock.is_some_and(|clock| {
                setting.current_scope(clock, now)
                    && clock.actions.as_ref().is_none_or(|actions| actions.seek)
                    && (clock.sequence <= setting.sequence || clock.actions.is_none())
            })
        {
            // A mode receipt can precede the refreshed capabilities for the
            // same occurrence. Keep the seek within its original deadline.
            return None;
        }
        let clock = clock.filter(|c| {
            c.generation == intent.generation
                && c.device_id == intent.device
                && intent.targeted_scope_matches(c)
                && (output_transport
                    || intent.control.operation_id().is_some()
                    || c.media_id.as_ref() == Some(&intent.media))
                && (!is_setting(&intent.control) || c.active && c.queue_epoch == intent.queue_epoch)
                && (intent.after_setting.is_none()
                    || after_setting.is_some_and(|setting| {
                        setting.current_scope(c, now) && c.sequence > setting.sequence
                    }))
                && fresh
                && (ordinarily_fresh
                    || extended_navigation.is_some_and(|(transition, _)| {
                        c.sequence > transition.sequence
                            && c.active
                            && (c.state == PlaybackState::Loading
                                || c.media_id
                                    .as_ref()
                                    .is_some_and(|media| media != &transition.media))
                    }))
        });
        let loading_followup_epoch = self
            .transition
            .as_ref()
            .zip(clock)
            .and_then(|(transition, clock)| transition.loading_followup_epoch(intent, clock, now));
        let rejected_loading_next = self
            .transition
            .as_ref()
            .filter(|transition| transition.rejected_loading_next(intent));
        let successor_pause_epoch = rejected_loading_next
            .zip(clock)
            .and_then(|(transition, clock)| transition.successor_pause_epoch(clock, now));
        if ordinarily_fresh && rejected_loading_next.is_some() && successor_pause_epoch.is_none() {
            return None;
        }
        if is_setting(&intent.control)
            && clock.is_some_and(|clock| {
                clock.active
                    && matches!(clock.state, PlaybackState::Playing | PlaybackState::Paused)
                    && clock.actions.is_none()
            })
        {
            // The option change briefly clears advertised actions on the same
            // recording. Wait for its complete clock within the original deadline.
            return None;
        }
        if output_transport
            && clock.is_some_and(|clock| {
                clock.actions.is_none()
                    || intent.requires_control_clock
                        && !playback_satisfied(clock, &intent.control)
                        && (clock.state != PlaybackState::Playing
                            || clock.rate <= 0.0
                            || !supported(clock, &RemoteOutputControl::Pause))
                    || extended_navigation.is_some_and(|(transition, loading_sequence)| {
                        clock.sequence <= loading_sequence
                            || clock
                                .media_id
                                .as_ref()
                                .is_none_or(|media| media == &transition.media)
                            || !matches!(
                                clock.state,
                                PlaybackState::Playing | PlaybackState::Paused
                            )
                            || clock.state == PlaybackState::Playing && clock.rate <= 0.0
                    })
                    || (is_playback(&intent.control)
                        && clock.state == PlaybackState::Loading
                        && !supported(clock, &intent.control))
                    || self.transition.as_ref().is_some_and(|transition| {
                        loading_followup_epoch.is_none() && !transition.is_settled(clock, now)
                    })
            })
        {
            // An ACK can precede the actual transition. The next user intent
            // must use the resulting track and advertised actions.
            return None;
        }
        let intent = self.queued.pop_front()?;
        if clock.is_some_and(|clock| playback_satisfied(clock, &intent.control)) {
            return None;
        }
        let Some(clock) = clock.filter(|c| supported(c, &intent.control)) else {
            if matches!(intent.control, RemoteOutputControl::AutoplayEnqueue { .. }) {
                return None;
            }
            self.queued.retain(|intent| {
                intent.control.operation_id().is_some() || is_output_transport(&intent.control)
            });
            return Some(RemoteUpdate::OutputCommandFailed {
                control: intent.control,
                error: unavailable(),
            });
        };
        let seek = intent.seek_barrier(clock, now);
        let observed_queue_epoch = if is_navigation(&intent.control) && intent.request_id.is_some()
        {
            clock.queue_epoch.filter(|epoch| *epoch > 0)
        } else {
            None
        };
        self.serial = self.serial.wrapping_add(1).max(1);
        let command = RemoteOutputCommand {
            id: self.serial,
            request_id: intent.request_id,
            expected_previous_media_id: None,
            observed_queue_epoch,
            required_queue_epoch: if intent.requires_control_clock {
                intent.queue_epoch
            } else {
                loading_followup_epoch.or(successor_pause_epoch)
            },
            generation: clock.generation,
            sequence: clock.sequence,
            device_id: intent.device,
            media_id: clock.media_id.clone()?,
            control: intent.control,
            expires_at_uptime_ms: clock.control_uptime_ms?.saturating_add(DISPATCH_WINDOW_MS),
        };
        if is_setting(&command.control) {
            self.setting = Some(SettingDispatch {
                command_id: command.id,
                control: command.control.clone(),
                generation: command.generation,
                device: command.device_id.clone(),
                media: command.media_id.clone(),
                queue_epoch: clock.queue_epoch,
                sequence: command.sequence,
                at: now,
                acknowledged: false,
            });
        }
        if output_transport {
            if is_navigation(&command.control)
                && let Some(followup) = self
                    .queued
                    .iter_mut()
                    .find(|intent| !matches!(intent.control, RemoteOutputControl::Volume { .. }))
                && followup.control == RemoteOutputControl::Pause
                && followup.generation == command.generation
                && followup.device == command.device_id
            {
                // This is the last preceding navigation in the admitted queue,
                // even when the burst's earlier steps changed the media ID.
                followup.after_navigation = Some(command.id);
            }
            let source_switch_pending = intent.may_start_from_paused
                || starts_from_paused(&command.control, clock)
                || self.transition.as_ref().is_some_and(|transition| {
                    transition
                        .source_switch_until
                        .is_some_and(|until| now < until)
                });
            self.transition = Some(TransportTransition {
                command_id: command.id,
                request_id: command.request_id,
                acknowledged: false,
                control: command.control.clone(),
                generation: command.generation,
                device: command.device_id.clone(),
                media: command.media_id.clone(),
                sequence: command.sequence,
                queue_epoch: clock.queue_epoch,
                loading_followup: loading_followup_epoch.is_some(),
                at: now,
                source_switch_until: source_switch_pending.then_some(now + PIPE_TIMEOUT),
                same_media_next: None,
            });
        }
        self.in_flight = Some(Sent {
            command: command.clone(),
            at: now,
            revoked: false,
            seek,
        });
        Some(RemoteUpdate::OutputCommand(command))
    }
}
