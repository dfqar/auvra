//! Account-bound selection restore. Storage and remote queues keep their own owners.
use super::*;
use std::time::{Duration, Instant};

const OWNED_PAUSE_CLOCK_FRESHNESS: Duration = Duration::from_secs(6);
const OWNED_PAUSE_PLAYING_FRESHNESS: Duration = Duration::from_millis(900);
const OWNED_PAUSE_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const OWNED_PAUSE_SETTLE_WINDOW: Duration = Duration::from_secs(8);
const CONFIRMED_RESUME_PAUSE_WINDOW: Duration = Duration::from_secs(2);

pub(super) struct ResumePending {
    pub(super) device_id: String,
    operation_id: Option<u64>,
    accepted: bool,
    observation_id: u64,
    started: Instant,
    from_owned_pause: bool,
}
pub(super) struct ConfirmedResumePause {
    operation_id: u64,
    device_id: String,
    selection: RemoteSelection,
    context_uri: Option<String>,
    position_ms: u64,
    confirmed_at: Instant,
}
impl ConfirmedResumePause {
    fn matches(&self, state: &RemoteState) -> bool {
        let age = self.confirmed_at.elapsed();
        age < CONFIRMED_RESUME_PAUSE_WINDOW
            && !state.authentication_required
            && state.connection == ProviderConnectionState::Connected
            && state.playback.as_ref().is_some_and(|frame| {
                frame.state == PlaybackState::Playing
                    && frame
                        .current
                        .as_ref()
                        .is_some_and(|item| item.id == self.selection.current.id)
                    && frame.context_uri == self.context_uri
                    && frame.device.as_ref().is_some_and(|device| {
                        device.id == self.device_id && device.is_active && !device.is_restricted
                    })
                    && !state
                        .devices
                        .iter()
                        .any(|device| device.id != self.device_id && device.is_active)
                    && frame.position_ms >= self.position_ms.saturating_sub(300)
                    && frame.position_ms
                        <= self
                            .position_ms
                            .saturating_add(age.as_millis() as u64 * 4)
                            .saturating_add(1500)
            })
    }
}
#[derive(Default)]
pub(super) struct OwnedPauseRecovery {
    selection: Option<(String, RemoteSelection)>,
    clock: Option<OwnedPauseClock>,
    watermark: (u64, u64),
    fence: Option<OwnedPauseFence>,
    discovery_gap: bool,
    pending_pause: Option<OwnedPauseIntent>,
}
enum OwnedPauseFence {
    NewPlayback,
    Seek { position_ms: u64, after: (u64, u64) },
}
struct OwnedPauseClock {
    generation: u64,
    device_id: String,
    media_id: MediaId,
    state: PlaybackState,
    moving_control_clock: bool,
    active: bool,
    position_ms: u64,
    received_at: Instant,
}
struct OwnedPauseIntent {
    device: String,
    selection: RemoteSelection,
    after: (u64, u64),
    deadline: Instant,
    confirmed: bool,
}
impl OwnedPauseRecovery {
    pub(super) fn clear(&mut self) {
        self.selection = None;
        self.clock = None;
        self.fence = Some(OwnedPauseFence::NewPlayback);
        self.discovery_gap = false;
        self.pending_pause = None;
    }
    fn matching_clock(
        &self,
        provider: &dyn RemoteProvider,
        device: &str,
        selection: &RemoteSelection,
    ) -> Option<&OwnedPauseClock> {
        self.clock.as_ref().filter(|clock| {
            clock.state == PlaybackState::Paused
                && clock.device_id == device
                && clock.received_at.elapsed() <= OWNED_PAUSE_CLOCK_FRESHNESS
                && clock.position_ms.abs_diff(selection.position_ms) <= 250
                && provider.output_media_matches(&clock.media_id, &selection.current.id)
        })
    }
    fn confirm_inactive_seek(&mut self) {
        let Some(OwnedPauseFence::Seek { position_ms, after }) = self.fence else {
            return;
        };
        let Some(clock) = self.clock.as_ref().filter(|clock| {
            clock.state == PlaybackState::Paused
                && !clock.active
                && self.watermark > after
                && clock.position_ms.abs_diff(position_ms) <= 250
        }) else {
            return;
        };
        let Some((device, selection)) = self.selection.as_mut() else {
            return;
        };
        if *device == clock.device_id {
            // This clock retained the exact identity of the previously verified
            // paused output. It can update its inert selection, never its lease.
            selection.position_ms = clock.position_ms.min(selection.current.duration_ms);
            self.fence = None;
        }
    }
    fn observe_pause_intent(&mut self, clock: &RemoteOutputClock, provider: &dyn RemoteProvider) {
        let Some(intent) = &mut self.pending_pause else {
            return;
        };
        let exact = clock.generation == intent.after.0
            && clock.sequence > intent.after.1
            && clock.device_id == intent.device
            && clock
                .media_id
                .as_ref()
                .is_some_and(|id| provider.output_media_matches(id, &intent.selection.current.id))
            && clock
                .metadata
                .as_ref()
                .and_then(|item| item.context_uri.as_ref())
                .is_none_or(|context| intent.selection.context_uri.as_ref() == Some(context));
        let inactive_pause = clock.state == PlaybackState::Paused && !clock.active;
        let now = Instant::now();
        if !exact
            || now >= intent.deadline
            || (intent.confirmed
                && (!inactive_pause
                    || clock.position_ms.abs_diff(intent.selection.position_ms) > 250))
        {
            self.pending_pause = None;
        } else if inactive_pause && !intent.confirmed {
            intent.selection.position_ms =
                clock.position_ms.min(intent.selection.current.duration_ms);
            intent.confirmed = true;
            intent.deadline = now + OWNED_PAUSE_SETTLE_WINDOW;
        }
    }
}
fn can_resume_on(state: &RemoteState, device_id: &str) -> bool {
    state
        .devices
        .iter()
        .any(|device| device.id == device_id && !device.is_restricted)
}
fn pause_target_conflicts(state: &RemoteState, device_id: &str) -> bool {
    state.devices.iter().any(|device| {
        if device.id == device_id {
            device.is_restricted
        } else {
            device.is_active && !device.is_restricted
        }
    })
}
fn matches_paused_selection(
    frame: &RemotePlayback,
    device_id: &str,
    selection: &RemoteSelection,
) -> bool {
    frame.state == PlaybackState::Paused
        && frame.context_uri == selection.context_uri
        && frame
            .device
            .as_ref()
            .is_some_and(|device| device.id == device_id && !device.is_restricted)
        && frame
            .current
            .as_ref()
            .is_some_and(|item| item.id == selection.current.id)
}
fn compatible_inactive_pause_frame(
    frame: &RemotePlayback,
    device_id: &str,
    selection: &RemoteSelection,
) -> bool {
    // An inactive endpoint can temporarily omit the item or context. Explicit
    // changes still supersede the previously verified selection.
    frame.state == PlaybackState::Paused
        && frame
            .device
            .as_ref()
            .is_some_and(|device| !device.is_active)
        && uncontradicted_pause_target(frame, device_id, selection)
}
fn uncontradicted_pause_target(
    frame: &RemotePlayback,
    device_id: &str,
    selection: &RemoteSelection,
) -> bool {
    frame
        .device
        .as_ref()
        .is_some_and(|device| device.id == device_id && !device.is_restricted)
        && frame
            .context_uri
            .as_ref()
            .is_none_or(|context| selection.context_uri.as_ref() == Some(context))
        && frame
            .current
            .as_ref()
            .is_none_or(|item| item.id == selection.current.id)
}
pub(super) struct RemoteAccount {
    profile_id: String,
    stable_id: Option<String>,
}
impl RemoteAccount {
    // A response without the immutable ID cannot verify a selection already bound to one.
    fn matches(&self, selected: &RemoteSelection) -> Option<bool> {
        match selected.stable_account_id.as_ref() {
            Some(saved) => self.stable_id.as_ref().map(|current| current == saved),
            None => Some(self.profile_id == selected.account_id),
        }
    }

    fn promote_verified_selection(&self, selected: &mut RemoteSelection) -> bool {
        if selected.stable_account_id.is_some() || self.stable_id.is_none() {
            return false;
        }
        debug_assert_eq!(self.matches(selected), Some(true));
        selected.stable_account_id.clone_from(&self.stable_id);
        true
    }
}
impl PlaybackCoordinator {
    pub(super) fn is_remote_selection_verified(&self, selected: &RemoteSelection) -> bool {
        self.remote
            .accounts
            .get(&selected.provider)
            .and_then(|account| account.matches(selected))
            == Some(true)
    }
    fn dormant_owned_pause(&self) -> Option<(&str, &RemoteSelection)> {
        let recovery = &self.remote.owned_pause;
        if recovery.fence.is_some() {
            return None;
        }
        let (device, selection) = recovery.selection.as_ref()?;
        let state = self.remote.states.get(&selection.provider)?;
        let provider = self.remote.providers.get(&selection.provider)?;
        let inactive = recovery
            .matching_clock(provider.as_ref(), device, selection)
            .is_some_and(|clock| !clock.active);
        let target_known = can_resume_on(state, device);
        let visible_connection = state.connection == ProviderConnectionState::Connected
            || (state.connection == ProviderConnectionState::Unavailable
                && target_known
                && inactive);
        let stale_owned_pause = state.playback.as_ref().is_some_and(|frame| {
            !state.owned_output_observed
                && inactive
                && (matches_paused_selection(frame, device, selection)
                    || compatible_inactive_pause_frame(frame, device, selection))
        });
        if self.remote.active.as_deref() == Some(selection.provider.as_str())
            && self.is_remote_selection_verified(selection)
            && !state.authentication_required
            && visible_connection
            && (state.playback.is_none() || stale_owned_pause)
            && !pause_target_conflicts(state, device)
            && (inactive || (!recovery.discovery_gap && target_known))
        {
            Some((device, selection))
        } else {
            None
        }
    }
    pub(super) fn remote_selection_needs_resume(&self) -> bool {
        self.remote.restored.is_some() || self.dormant_owned_pause().is_some()
    }
    fn pending_owned_pause(&self, provider: &str) -> Option<OwnedPauseIntent> {
        let recovery = &self.remote.owned_pause;
        let state = self.remote.states.get(provider)?;
        let source = self.remote.providers.get(provider)?;
        let frame = state.playback.as_ref()?;
        let (device, item) = frame.device.as_ref().zip(frame.current.as_ref())?;
        let clock = recovery.clock.as_ref()?;
        let mut selection = self.remote.last_selection.as_ref()?.clone();
        if self.remote.restored.is_some()
            || !state.owned_output_observed
            || !state.owned_output_control
            || state.authentication_required
            || frame.state != PlaybackState::Playing
            || frame.position_rate <= 0.0
            || !device.is_active
            || device.is_restricted
            || pause_target_conflicts(state, &device.id)
            || !clock.active
            || clock.state != PlaybackState::Playing
            || !clock.moving_control_clock
            || clock.received_at.elapsed() > OWNED_PAUSE_PLAYING_FRESHNESS
            || clock.device_id != device.id
            || !source.output_media_matches(&clock.media_id, &item.id)
            || selection.provider != provider
            || selection.current.id != item.id
            || selection.context_uri != frame.context_uri
            || !self.is_remote_selection_verified(&selection)
        {
            return None;
        }
        selection.current.artwork.clone_from(&item.artwork);
        Some(OwnedPauseIntent {
            device: device.id.clone(),
            selection,
            after: recovery.watermark,
            deadline: Instant::now() + OWNED_PAUSE_CONFIRM_WINDOW,
            confirmed: false,
        })
    }

    fn reconcile_owned_pause_intent(&mut self, provider: &str, state: &RemoteState) {
        let recovery = &self.remote.owned_pause;
        let Some(intent) = recovery.pending_pause.as_ref() else {
            return;
        };
        if !self.is_remote_selection_verified(&intent.selection)
            || pause_target_conflicts(state, &intent.device)
            || state.playback.as_ref().is_some_and(|frame| {
                !uncontradicted_pause_target(frame, &intent.device, &intent.selection)
            })
            || Instant::now() >= intent.deadline
        {
            self.remote.owned_pause.pending_pause = None;
            return;
        }
        // A late HTTP Playing frame for the same item may precede the empty
        // playback response. Hold the proof without projecting a pause over it.
        if !intent.confirmed
            || state.owned_output_observed
            || !can_resume_on(state, &intent.device)
            || state.playback.as_ref().is_some_and(|frame| {
                !matches_paused_selection(frame, &intent.device, &intent.selection)
                    && !compatible_inactive_pause_frame(frame, &intent.device, &intent.selection)
            })
            || !self.remote.providers.get(provider).is_some_and(|source| {
                recovery
                    .matching_clock(source.as_ref(), &intent.device, &intent.selection)
                    .is_some_and(|clock| !clock.active)
            })
        {
            return;
        }
        let recovery = &mut self.remote.owned_pause;
        let intent = recovery.pending_pause.take().unwrap();
        let mut checkpoint = intent.selection.clone();
        checkpoint.current.artwork = None;
        self.remote.last_selection = Some(checkpoint);
        self.session_dirty = true;
        recovery.selection = Some((intent.device, intent.selection));
        recovery.fence = None;
        recovery.discovery_gap = false;
    }

    pub(super) fn observe_owned_pause(&mut self, provider: &str, state: &RemoteState) {
        if self.remote.active.as_deref() != Some(provider) {
            return;
        }
        if state.authentication_required
            || matches!(
                state.connection,
                ProviderConnectionState::Disconnected | ProviderConnectionState::Connecting
            )
        {
            self.remote.owned_pause.clear();
            return;
        }
        self.reconcile_owned_pause_intent(provider, state);
        let recovery = &mut self.remote.owned_pause;
        if let Some((device, selection)) = recovery.selection.as_ref() {
            let has_recent_clock = self.remote.providers.get(provider).is_some_and(|source| {
                recovery
                    .matching_clock(source.as_ref(), device, selection)
                    .is_some()
            });
            let compatible_frame = state.playback.as_ref().is_none_or(|frame| {
                matches_paused_selection(frame, device, selection)
                    || (has_recent_clock
                        && compatible_inactive_pause_frame(frame, device, selection))
            });
            if !state.owned_output_observed && pause_target_conflicts(state, device) {
                recovery.clear();
            } else if !compatible_frame {
                recovery.selection = None;
            } else if state.connection == ProviderConnectionState::Connected
                && !state.owned_output_observed
                && !can_resume_on(state, device)
            {
                if has_recent_clock {
                    // Keep the prior proof hidden until an exact inactive clock
                    // arrives. Discovery must return before a mutation is allowed.
                    recovery.discovery_gap = true;
                } else {
                    recovery.clear();
                }
            }
        }
        let Some(frame) = state.playback.as_ref() else {
            return;
        };
        let target = frame.device.as_ref().zip(frame.current.as_ref());
        let owned = state.owned_output_observed
            && target.is_some_and(|(device, _)| device.is_active && !device.is_restricted);
        if owned && frame.state == PlaybackState::Playing {
            recovery.fence = None;
        }
        if !owned || frame.state != PlaybackState::Paused || self.remote.restored.is_some() {
            return;
        }
        let Some((device, item)) = target else { return };
        let Some(clock) = recovery.clock.as_ref().filter(|clock| {
            clock.state == PlaybackState::Paused
                && clock.active
                && clock.device_id == device.id
                && clock.position_ms.abs_diff(frame.position_ms) <= 250
                && self.remote.providers.get(provider).is_some_and(|provider| {
                    provider.output_media_matches(&clock.media_id, &item.id)
                })
        }) else {
            return;
        };
        if let Some(fence) = &recovery.fence
            && !matches!(fence, OwnedPauseFence::Seek { position_ms, after }
                if recovery.watermark > *after
                    && clock.position_ms.abs_diff(*position_ms) <= 250
                    && frame.position_ms.abs_diff(*position_ms) <= 250)
        {
            return;
        }
        let selected = self.remote.last_selection.as_ref().filter(|saved| {
            saved.provider == provider
                && saved.current.id == item.id
                && saved.context_uri == frame.context_uri
                && saved.position_ms == frame.position_ms.min(item.duration_ms)
        });
        if let Some(selected) = selected
            && self.is_remote_selection_verified(selected)
        {
            let mut selection = selected.clone();
            selection.current.artwork.clone_from(&item.artwork);
            self.remote.owned_pause.selection = Some((device.id.clone(), selection));
            self.remote.owned_pause.fence = None;
            self.remote.owned_pause.discovery_gap = false;
            self.remote.owned_pause.pending_pause = None;
        }
    }
    pub(super) fn note_owned_pause_action(&mut self, provider: &str, action: &RemoteAction) {
        if self.remote.active.as_deref() != Some(provider) {
            return;
        }
        let pending = match action {
            RemoteAction::Control {
                control: RemoteControl::Pause,
            } => self.pending_owned_pause(provider),
            _ => None,
        };
        let recovery = &mut self.remote.owned_pause;
        match action {
            RemoteAction::Credential { .. } | RemoteAction::Disconnect => recovery.clear(),
            RemoteAction::OutputClock { clock: None } => recovery.clear(),
            RemoteAction::OutputClock { clock: Some(clock) } => {
                if (clock.generation, clock.sequence) <= recovery.watermark {
                    return;
                }
                recovery.watermark = (clock.generation, clock.sequence);
                if let Some(source) = self.remote.providers.get(provider) {
                    recovery.observe_pause_intent(clock, source.as_ref());
                }
                recovery.clock = clock
                    .media_id
                    .as_ref()
                    .map(|media_id| OwnedPauseClock {
                        generation: clock.generation,
                        device_id: clock.device_id.clone(),
                        media_id: media_id.clone(),
                        state: clock.state,
                        moving_control_clock: clock.rate > 0.0 && clock.control_uptime_ms.is_some(),
                        active: clock.active,
                        position_ms: clock.position_ms,
                        received_at: Instant::now(),
                    })
                    .inspect(|next| {
                        if recovery.clock.as_ref().is_some_and(|old| {
                            old.generation != next.generation
                                || old.device_id != next.device_id
                                || old.media_id != next.media_id
                        }) {
                            recovery.selection = None;
                        }
                    });
                if clock.state != PlaybackState::Paused || recovery.clock.is_none() {
                    recovery.selection = None
                }
                recovery.confirm_inactive_seek();
            }
            RemoteAction::Control {
                control: RemoteControl::Pause | RemoteControl::PauseOnDevice { .. },
            } => {
                recovery.pending_pause = pending;
                if !self
                    .remote
                    .resume_pending
                    .as_ref()
                    .is_some_and(|pending| pending.from_owned_pause)
                {
                    recovery.selection = None;
                }
                recovery.fence = None;
            }
            RemoteAction::Control {
                control: RemoteControl::Seek { position_ms },
            } => {
                recovery.pending_pause = None;
                recovery.fence = Some(OwnedPauseFence::Seek {
                    position_ms: *position_ms,
                    after: recovery.watermark,
                });
            }
            RemoteAction::Activate
            | RemoteAction::Control {
                control:
                    RemoteControl::Play
                    | RemoteControl::Stop
                    | RemoteControl::Transfer { .. }
                    | RemoteControl::Start { .. }
                    | RemoteControl::StartOnDevice { .. }
                    | RemoteControl::StartSpokenOnDevice { .. }
                    | RemoteControl::StartContext { .. }
                    | RemoteControl::Next
                    | RemoteControl::Previous,
            } => {
                recovery.pending_pause = None;
                recovery.selection = None;
                recovery.fence = Some(OwnedPauseFence::NewPlayback);
            }
            RemoteAction::Control {
                control: RemoteControl::ResumeOnDevice { .. },
            } => recovery.pending_pause = None,
            _ => {}
        }
    }
    pub(super) fn remote_selection_for_storage(&self) -> Option<RemoteSelection> {
        let mut selected = self
            .remote
            .restored
            .clone()
            .or_else(|| self.remote.candidate.clone())
            .or_else(|| {
                self.remote
                    .last_selection
                    .as_ref()
                    .filter(|s| self.remote.active.as_deref() == Some(&s.provider))
                    .cloned()
            })?;
        if self.remote.restored.is_none()
            && self.remote.candidate.is_none()
            && self.is_remote_selection_verified(&selected)
            && let Some(snapshot) = self.remote_snapshot()
            && snapshot
                .queue
                .current
                .as_ref()
                .is_some_and(|i| i.id == selected.current.id)
        {
            // The live clock can reach the duration before the provider reports
            // a transition. Keep the observed offset instead of saving a
            // synthetic end position. A paused offset remains authoritative.
            if snapshot.state != PlaybackState::Playing
                || snapshot.position_ms < selected.current.duration_ms
            {
                selected.position_ms = snapshot.position_ms;
            } else if let Some(frame) = self
                .remote
                .states
                .get(&selected.provider)
                .and_then(|state| state.playback.as_ref())
                .filter(|frame| {
                    frame.state == PlaybackState::Playing
                        && frame.position_ms < selected.current.duration_ms
                        && frame
                            .current
                            .as_ref()
                            .is_some_and(|item| item.id == selected.current.id)
                })
            {
                selected.position_ms = frame.position_ms;
            }
        }
        selected.current.artwork = None;
        selected.validate().ok()?;
        Some(selected)
    }
    pub(super) fn maybe_restore_remote(&mut self) {
        let Some(candidate) = self.remote.candidate.as_ref() else {
            return;
        };
        let Some(account) = self.remote.accounts.get(&candidate.provider) else {
            return;
        };
        match account.matches(candidate) {
            None => return,
            Some(false) => {
                self.remote.candidate = None;
                self.remote.last_selection = None;
                self.session_dirty = true;
                return;
            }
            Some(true) => {}
        }
        if !self.session_state.ready
            || self.provider_loaded
            || self.remote.active.is_some()
            || !self.remote.providers.contains_key(&candidate.provider)
        {
            return;
        }
        self.remote.local_stopped = Some(matches!(
            self.state,
            PlaybackState::Stopped | PlaybackState::Idle
        ));
        self.remote.active = Some(candidate.provider.clone());
        self.remote.restored = self.remote.candidate.take().map(|mut selected| {
            account.promote_verified_selection(&mut selected);
            selected
        });
        self.active = None;
        self.wanted_playing = false;
        self.session_state.restored = true;
        self.session_dirty = true;
    }
    pub(super) fn restored_remote_snapshot(&self) -> Option<PlayerSnapshot> {
        let dormant = self.dormant_owned_pause();
        let selected = self
            .remote
            .restored
            .as_ref()
            .or_else(|| dormant.map(|(_, saved)| saved))?;
        if self.remote.active.as_deref() != Some(&selected.provider)
            || !self.is_remote_selection_verified(selected)
        {
            return None;
        }
        let remote_state = self.remote.states.get(&selected.provider);
        let connection = remote_state.map_or(ProviderConnectionState::Connecting, |s| s.connection);
        let has_runtime_target = self
            .remote
            .resume_pending
            .as_ref()
            .filter(|pending| pending.from_owned_pause)
            .map(|pending| pending.device_id.as_str())
            .or_else(|| {
                let (device, _) = dormant?;
                self.remote.restored.is_none().then_some(device)
            })
            .or_else(|| self.interrupted_owned_device())
            .is_none_or(|device| remote_state.is_some_and(|state| can_resume_on(state, device)));
        Some(PlayerSnapshot {
            host_analysis_status: None,
            host_spatial_status: None,
            host_analysis_revoked_through: self.host_analysis_revoked_through(),
            enhancement: None,
            restored_remote: Some(RestoredRemote {
                context_uri: selected.context_uri.clone(),
            }),
            revision: self.revision,
            state: PlaybackState::Paused,
            position_ms: selected.position_ms,
            playback_rate: 1.0,
            volume: self.volume,
            sound: self.sound.clone(),
            output_statistics: None,
            active_provider: Some(selected.provider.clone()),
            provider_connection: connection,
            capabilities: Capabilities {
                play: connection == ProviderConnectionState::Connected
                    && selected.current.capabilities.play
                    && has_runtime_target,
                ..Default::default()
            },
            transport: TransportControls::default(),
            playback_session_active: false,
            queue: QueueSnapshot {
                revision: self.revision,
                context: PlaybackContext::UserQueue,
                current: Some(selected.current.clone()),
                upcoming: vec![],
                history: vec![],
                device_queue: None,
                repeat: RepeatMode::Off,
                shuffle: false,
                play_next_count: 0,
                forward_count: 0,
            },
            session: self.session_state.clone(),
            queue_issues: vec![],
        })
    }
    pub(super) fn hide_restored_remote(&mut self, id: &str) {
        self.remote.owned_recovery.clear_interrupted();
        if self
            .remote
            .restored
            .as_ref()
            .is_some_and(|s| s.provider == id)
        {
            self.remote.candidate = self.remote.restored.take();
            self.remote.active = None;
            self.active = self.queue.current().map(|i| i.id.provider().to_owned());
            self.state = if self.active.is_some() {
                PlaybackState::Paused
            } else {
                PlaybackState::Idle
            };
            self.provider_loaded = false;
        }
        self.remote.resume_pending = None;
    }
    pub(super) fn cancel_remote_restore_for_local(&mut self) {
        self.remote.owned_recovery.clear_interrupted();
        self.remote.owned_pause.clear();
        self.remote.restore_cancelled = true;
        self.remote.candidate = None;
        if self.remote.restored.is_some() && self.remote.resume_pending.is_none() {
            self.remote.restored = None;
            self.remote.active = None;
            self.remote.last_selection = None;
            self.active = self.queue.current().map(|i| i.id.provider().to_owned());
            self.session_dirty = true;
        }
    }
    pub(super) fn accept_remote_account_page(&mut self, page: &RemotePage) {
        if !page.detail || page.issue.is_some() || page.collections.len() != 1 {
            return;
        }
        let profile = &page.collections[0];
        if profile.kind != RemoteEntityKind::Profile
            || profile.id.is_empty()
            || profile.id.len() > 256
            || profile.id.chars().any(char::is_control)
        {
            return;
        }
        let account = RemoteAccount {
            profile_id: profile.id.clone(),
            stable_id: profile
                .account_id
                .as_ref()
                .filter(|id| valid_stable_account_id(id))
                .cloned()
                .or_else(|| {
                    // Credential replacement and authentication failure clear this registry.
                    self.remote
                        .accounts
                        .get(&page.provider)
                        .filter(|known| known.profile_id == profile.id)
                        .and_then(|known| known.stable_id.clone())
                }),
        };
        let recovery = &mut self.remote.owned_pause;
        let contradicts_account = |saved: &RemoteSelection| {
            saved.provider == page.provider && account.matches(saved) != Some(true)
        };
        if recovery
            .selection
            .as_ref()
            .is_some_and(|(_, saved)| contradicts_account(saved))
            || recovery
                .pending_pause
                .as_ref()
                .is_some_and(|intent| contradicts_account(&intent.selection))
        {
            recovery.clear();
        } else if let Some(intent) = recovery
            .pending_pause
            .as_mut()
            .filter(|intent| intent.selection.provider == page.provider)
        {
            account.promote_verified_selection(&mut intent.selection);
        }
        if let Some(selected) = self
            .remote
            .last_selection
            .as_mut()
            .filter(|selected| selected.provider == page.provider)
        {
            match account.matches(selected) {
                Some(false) => {
                    self.remote.last_selection = None;
                    self.session_dirty = true;
                }
                Some(true) => self.session_dirty |= account.promote_verified_selection(selected),
                None => {}
            }
        }
        if let Some(selected) = self
            .remote
            .restored
            .as_mut()
            .filter(|selected| selected.provider == page.provider)
        {
            if account.matches(selected) == Some(true) {
                self.session_dirty |= account.promote_verified_selection(selected);
            } else {
                self.hide_restored_remote(&page.provider);
            }
        }
        self.remote.accounts.insert(page.provider.clone(), account);
        self.reconcile_owned_recovery_account(&page.provider);
        if self.remote.active.as_ref() == Some(&page.provider) {
            self.remote.owned_recovery.account_verified();
        }
        self.maybe_restore_remote();
        if let Some(state) = self.remote.states.get(&page.provider).cloned() {
            self.capture_remote_selection(&page.provider, &state);
        }
        self.publish_snapshot();
    }
    pub(super) fn capture_remote_selection(&mut self, id: &str, state: &RemoteState) {
        if self.remote.active.as_deref() != Some(id)
            || self.remote.restored.is_some()
            || state.connection != ProviderConnectionState::Connected
        {
            return;
        }
        if let Some((_, selection)) = self.dormant_owned_pause() {
            // The verified inactive output can be newer than Spotify's paused HTTP frame.
            let checkpoint = self.remote.last_selection.as_ref().and_then(|saved| {
                (saved.provider == selection.provider
                    && saved.current.id == selection.current.id
                    && saved.context_uri == selection.context_uri
                    && saved.position_ms != selection.position_ms)
                    .then_some(selection.position_ms)
            });
            if let (Some(position_ms), Some(saved)) = (checkpoint, &mut self.remote.last_selection)
            {
                saved.position_ms = position_ms;
                self.session_dirty = true;
            }
            return;
        }
        let Some(account) = self.remote.accounts.get(id) else {
            return;
        };
        if account.stable_id.is_none()
            && self
                .remote
                .candidate
                .iter()
                .chain(self.remote.last_selection.iter())
                .any(|selected| selected.provider == id && selected.stable_account_id.is_some())
        {
            // Keep an immutable checkpoint intact until a new credential verifies it.
            return;
        }
        let Some(frame) = state.playback.as_ref() else {
            return;
        };
        if let Some((device, selection)) = self.remote.owned_pause.selection.as_ref()
            && !matches_paused_selection(frame, device, selection)
            && compatible_inactive_pause_frame(frame, device, selection)
            && self.remote.providers.get(id).is_some_and(|provider| {
                self.remote
                    .owned_pause
                    .matching_clock(provider.as_ref(), device, selection)
                    .is_some()
            })
        {
            return;
        }
        if !matches!(frame.state, PlaybackState::Playing | PlaybackState::Paused) {
            return;
        }
        let Some(mut current) = frame.current.clone() else {
            return;
        };
        current.artwork = None;
        let selected = RemoteSelection {
            provider: id.to_owned(),
            account_id: account.profile_id.clone(),
            stable_account_id: account.stable_id.clone(),
            position_ms: frame.position_ms.min(current.duration_ms),
            current,
            context_uri: frame.context_uri.clone(),
        };
        if selected.validate().is_ok() {
            self.remote.last_selection = Some(selected);
            self.session_dirty = true;
        }
    }
    pub(super) fn update_restored_metadata(&mut self, page: &RemotePage) {
        if !page.detail || page.issue.is_some() {
            return;
        }
        let Some(selected) = self
            .remote
            .restored
            .as_mut()
            .filter(|s| s.provider == page.provider)
        else {
            return;
        };
        let Some(item) = page.items.iter().find(|i| i.id == selected.current.id) else {
            return;
        };
        let mut updated = selected.clone();
        updated.current = item.clone();
        updated.position_ms = updated.position_ms.min(item.duration_ms);
        if updated.validate().is_ok() {
            *selected = updated;
            self.session_dirty = true;
            self.publish_snapshot();
        }
    }
    pub(super) fn validate_remote_resume(
        &self,
        provider: &str,
        action: &RemoteAction,
    ) -> Result<(), PlayerError> {
        if let RemoteAction::Control {
            control:
                RemoteControl::ResumeOnDevice {
                    item,
                    device_id,
                    position_ms,
                    context_uri,
                    ..
                },
        } = action
        {
            let dormant = self.dormant_owned_pause();
            let owned_device = if self.remote.restored.is_none() {
                dormant.map(|(device, _)| device)
            } else {
                self.remote
                    .resume_pending
                    .as_ref()
                    .filter(|pending| pending.from_owned_pause)
                    .map(|pending| pending.device_id.as_str())
            }
            .or_else(|| self.interrupted_owned_device());
            let selected = self
                .remote
                .restored
                .as_ref()
                .or_else(|| dormant.map(|(_, selected)| selected))
                .ok_or_else(|| {
                    PlayerError::invalid("This saved selection is no longer available")
                })?;
            self.require_capability(self.snapshot().capabilities.play)?;
            if selected.provider != provider
                || selected.current.id != item.id
                || selected.position_ms != *position_ms
                || selected.context_uri != *context_uri
                || owned_device.is_some_and(|device| device != device_id)
                || !self.is_remote_selection_verified(selected)
            {
                return Err(PlayerError::invalid(
                    "The saved selection changed before Resume",
                ));
            }
        } else if self.remote_selection_needs_resume()
            && matches!(
                action,
                RemoteAction::Control {
                    control: RemoteControl::Play
                        | RemoteControl::Seek { .. }
                        | RemoteControl::Volume { .. }
                        | RemoteControl::Enqueue { .. }
                        | RemoteControl::Next
                        | RemoteControl::Previous
                        | RemoteControl::Shuffle { .. }
                        | RemoteControl::Repeat { .. }
                }
            )
        {
            return Err(PlayerError::new(
                ErrorCode::UnsupportedCapability,
                "Start this saved selection on a playback device first",
            ));
        }
        Ok(())
    }
    pub(super) fn note_remote_action(&mut self, provider: &str, action: &RemoteAction) {
        self.note_owned_pause_action(provider, action);
        match action {
            RemoteAction::Credential { .. } => {
                self.remote.accounts.remove(provider);
                self.remote.states.remove(provider);
                self.remote.anchors.remove(provider);
                self.hide_restored_remote(provider);
            }
            RemoteAction::Disconnect => {
                self.remote.owned_recovery.clear_interrupted();
                self.remote.accounts.remove(provider);
                self.remote.candidate = None;
                self.remote.restored = None;
                self.remote.last_selection = None;
                self.remote.resume_pending = None;
                self.remote.restore_cancelled = true;
                self.session_dirty = true;
            }
            RemoteAction::Control {
                control:
                    RemoteControl::ResumeOnDevice {
                        device_id,
                        operation_id,
                        ..
                    },
            } => {
                let from_owned_pause = self.remote.restored.is_none()
                    || self
                        .remote
                        .resume_pending
                        .as_ref()
                        .is_some_and(|pending| pending.from_owned_pause);
                if self.remote.restored.is_none() {
                    self.remote.restored = self
                        .dormant_owned_pause()
                        .map(|(_, selection)| selection.clone());
                }
                self.remote.resume_pending = Some(ResumePending {
                    device_id: device_id.clone(),
                    operation_id: *operation_id,
                    accepted: operation_id.is_none(),
                    observation_id: self
                        .remote
                        .states
                        .get(provider)
                        .map_or(0, |s| s.observation_id),
                    started: Instant::now(),
                    from_owned_pause,
                });
                self.remote.restore_cancelled = true;
                self.remote.candidate = None;
            }
            RemoteAction::Activate | RemoteAction::Control { .. } => {
                self.remote.restore_cancelled = true;
                self.remote.candidate = None;
                let pausing = matches!(
                    action,
                    RemoteAction::Control {
                        control: RemoteControl::Pause | RemoteControl::PauseOnDevice { .. }
                    }
                );
                if !pausing
                    || self
                        .remote
                        .resume_pending
                        .as_ref()
                        .is_some_and(|pending| pending.from_owned_pause)
                {
                    self.remote.restored = None;
                    self.remote.owned_recovery.clear_interrupted();
                    self.session_state.restored = false;
                }
                self.remote.resume_pending = None;
                self.session_dirty = true;
            }
            _ => {}
        }
    }
    pub(super) fn confirm_remote_resume(&mut self, id: &str, state: &RemoteState) {
        if self
            .remote
            .confirmed_resume_pause
            .as_ref()
            .is_some_and(|scope| scope.selection.provider == id && !scope.matches(state))
        {
            self.remote.confirmed_resume_pause = None;
        }
        let (Some(selected), Some(pending)) =
            (&self.remote.restored, &mut self.remote.resume_pending)
        else {
            return;
        };
        if selected.provider != id {
            return;
        }
        if pending.from_owned_pause
            && state.connection == ProviderConnectionState::Connected
            && !state.authentication_required
            && state.playback.as_ref().is_some_and(|frame| {
                frame
                    .current
                    .as_ref()
                    .zip(frame.device.as_ref())
                    .is_some_and(|(item, device)| {
                        device.is_active
                            && !device.is_restricted
                            && (device.id != pending.device_id
                                || self.remote.providers.get(id).is_some_and(|provider| {
                                    !provider.output_media_matches(&item.id, &selected.current.id)
                                }))
                    })
            })
        {
            self.remote.restored = None;
            self.remote.resume_pending = None;
            return;
        }
        // Acceptance and playback observations can arrive in either order. Keep
        // the current acceptance when later observations carry no result or an
        // outcome from another operation.
        if let Some(result) = &state.control_result
            && Some(result.operation_id) == pending.operation_id
        {
            if result.error.is_some() {
                if pending.from_owned_pause {
                    self.remote.restored = None;
                }
                self.remote.resume_pending = None;
                return;
            }
            pending.accepted = true;
        }
        let Some(frame) = state.playback.as_ref().filter(|_| pending.accepted) else {
            return;
        };
        let elapsed = (pending.started.elapsed().as_secs_f64() * 1000.0 * frame.position_rate)
            .min(u64::MAX as f64) as u64;
        if state.observation_id != pending.observation_id
            && state.connection == ProviderConnectionState::Connected
            && frame.state == PlaybackState::Playing
            && frame
                .current
                .as_ref()
                .is_some_and(|i| i.id == selected.current.id)
            && frame
                .device
                .as_ref()
                .is_some_and(|d| d.id == pending.device_id)
            && frame.position_ms >= selected.position_ms.saturating_sub(2000)
            && frame.position_ms
                <= selected
                    .position_ms
                    .saturating_add(elapsed)
                    .saturating_add(4000)
        {
            self.remote.confirmed_resume_pause =
                pending
                    .operation_id
                    .filter(|id| *id != 0)
                    .map(|operation_id| ConfirmedResumePause {
                        operation_id,
                        device_id: pending.device_id.clone(),
                        selection: selected.clone(),
                        context_uri: frame.context_uri.clone(),
                        position_ms: frame.position_ms,
                        confirmed_at: Instant::now(),
                    });
            self.remote.restored = None;
            self.remote.resume_pending = None;
            self.remote.owned_recovery.resumed();
            self.session_state.restored = false;
            self.session_dirty = true;
        }
    }

    pub(super) fn pause_confirmed_restored_resume(
        &mut self,
        operation_id: u64,
        device_id: &str,
        request_id: Option<u64>,
    ) -> Result<(), PlayerError> {
        let stale = || PlayerError::invalid("The resumed playback selection changed before Pause");
        let scope = self
            .remote
            .confirmed_resume_pause
            .as_ref()
            .filter(|scope| scope.operation_id == operation_id && scope.device_id == device_id)
            .ok_or_else(stale)?;
        if self.remote.active.as_ref() != Some(&scope.selection.provider)
            || !self.is_remote_selection_verified(&scope.selection)
            || !self
                .remote
                .states
                .get(&scope.selection.provider)
                .is_some_and(|state| scope.matches(state))
        {
            self.remote.confirmed_resume_pause = None;
            return Err(stale());
        }
        let provider_id = scope.selection.provider.clone();
        self.remote.confirmed_resume_pause = None;
        let action = RemoteAction::Control {
            control: RemoteControl::PauseOnDevice {
                device_id: device_id.into(),
            },
        };
        let native = if let Some(request_id) = request_id {
            self.remote
                .providers
                .get_mut(&provider_id)
                .ok_or_else(|| PlayerError::unavailable(&provider_id))?
                .submit_owned_pause_after_resume(operation_id, device_id, request_id)?
        } else {
            false
        };
        if !native {
            self.submit_remote(&provider_id, action.clone())?;
        }
        self.cancel_deferred_spatial_play();
        self.clear_host_analysis();
        self.note_remote_action(&provider_id, &action);
        Ok(())
    }

    pub(super) fn cancel_restored_resume(
        &mut self,
        operation_id: u64,
        device_id: &str,
    ) -> Result<(), PlayerError> {
        let unavailable = || {
            PlayerError::new(
                ErrorCode::UnsupportedCapability,
                "This Resume can no longer be cancelled before dispatch",
            )
        };
        let selected = self.remote.restored.as_ref().ok_or_else(unavailable)?;
        let pending = self
            .remote
            .resume_pending
            .as_ref()
            .filter(|pending| {
                operation_id != 0
                    && pending.operation_id == Some(operation_id)
                    && pending.device_id == device_id
                    && !pending.accepted
            })
            .ok_or_else(unavailable)?;
        if self.remote.active.as_ref() != Some(&selected.provider)
            || !self.is_remote_selection_verified(selected)
        {
            return Err(unavailable());
        }
        let from_owned_pause = pending.from_owned_pause;
        let cancelled = self
            .remote
            .providers
            .get_mut(&selected.provider)
            .is_some_and(|provider| {
                provider.cancel_resume_before_dispatch(operation_id, device_id)
            });
        if !cancelled {
            return Err(unavailable());
        }
        self.remote.resume_pending = None;
        if from_owned_pause {
            self.remote.restored = None;
        }
        self.session_dirty = true;
        Ok(())
    }
}
