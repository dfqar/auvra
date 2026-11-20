use super::*;
use std::time::Instant;

struct LocalSwitch {
    command: PlayerCommand,
    provider: String,
    pause: Option<PauseConfirmation>,
    deadline: Instant,
}
struct PauseConfirmation {
    observation_id: u64,
    target_device: Option<String>,
    owned: bool,
}

#[derive(Default)]
pub(super) struct RemoteRegistry {
    pub(super) active: Option<String>,
    pub(super) providers: BTreeMap<String, Box<dyn RemoteProvider>>,
    pub(super) states: BTreeMap<String, RemoteState>,
    pub(super) anchors: BTreeMap<String, (u64, Instant)>,
    pending_local: Option<LocalSwitch>,
    pub(super) accounts: BTreeMap<String, super::remote_session::RemoteAccount>,
    pub(super) candidate: Option<RemoteSelection>,
    pub(super) restored: Option<RemoteSelection>,
    pub(super) last_selection: Option<RemoteSelection>,
    pub(super) owned_pause: super::remote_session::OwnedPauseRecovery,
    pub(super) owned_recovery: super::owned_recovery::OwnedOutputRecovery,
    pub(super) resume_pending: Option<super::remote_session::ResumePending>,
    pub(super) confirmed_resume_pause: Option<super::remote_session::ConfirmedResumePause>,
    pub(super) restore_cancelled: bool,
    pub(super) local_stopped: Option<bool>,
}
impl PlaybackCoordinator {
    pub fn register_remote(&mut self, provider: impl RemoteProvider + 'static) {
        self.remote
            .providers
            .insert(provider.id().to_owned(), Box::new(provider));
    }
    pub(super) fn submit_remote(
        &mut self,
        id: &str,
        action: RemoteAction,
    ) -> Result<(), PlayerError> {
        self.remote
            .providers
            .get_mut(id)
            .ok_or_else(|| PlayerError::unavailable(id))?
            .submit(action)
    }
    fn activate_remote(&mut self, id: &str) -> Result<(), PlayerError> {
        if self.remote.active.as_deref() == Some(id) {
            return Ok(());
        }
        if let Some(previous) = self.remote.active.clone() {
            self.cancel_owned_output_recovery(&previous)?;
        }
        self.checkpoint(true);
        self.remote.local_stopped = Some(matches!(
            self.state,
            PlaybackState::Stopped | PlaybackState::Idle
        ));
        if self.provider_loaded {
            self.active_mut()?.stop()?;
        }
        self.provider_loaded = false;
        self.wanted_playing = false;
        self.state = PlaybackState::Stopped;
        self.active = None;
        self.remote.owned_pause = Default::default();
        self.remote.active = Some(id.to_owned());
        Ok(())
    }
    pub(super) fn leave_remote(&mut self) -> Result<(), PlayerError> {
        if self
            .remote
            .active
            .as_ref()
            .and_then(|id| self.remote.states.get(id))
            .and_then(|s| s.playback.as_ref())
            .is_some_and(|p| p.state == PlaybackState::Playing)
        {
            return Err(PlayerError::invalid(
                "Wait for remote playback to pause before starting local audio",
            ));
        }
        self.remote.owned_pause.clear();
        self.remote.active = None;
        Ok(())
    }
    /// A local source may start only after a fresh remote observation confirms Pause.
    /// This defers the whole intent; it never blocks the coordinator or mixes sources.
    pub(super) fn defer_local_switch(
        &mut self,
        command: &PlayerCommand,
    ) -> Option<Result<(), PlayerError>> {
        if !matches!(
            command,
            PlayerCommand::OpenFile { .. }
                | PlayerCommand::ReplaceQueue { .. }
                | PlayerCommand::PlayLibrary { .. }
        ) {
            return None;
        }
        let id = self.remote.active.clone()?;
        if self
            .remote
            .providers
            .get(&id)
            .is_some_and(|p| p.has_pending_output_start())
        {
            self.remote.pending_local = Some(LocalSwitch {
                command: command.clone(),
                provider: id,
                pause: None,
                deadline: Instant::now() + Duration::from_secs(10),
            });
            return Some(Ok(()));
        }
        let state = self.remote.states.get(&id).cloned();
        let resuming = self.remote.resume_pending.is_some();
        if !resuming
            && state.as_ref().is_some_and(|s| {
                s.transport_available()
                    && s.playback.as_ref().is_none_or(|p| {
                        !matches!(p.state, PlaybackState::Playing | PlaybackState::Loading)
                    })
            })
        {
            return None;
        }
        let result = (|| {
            let state = state.ok_or_else(|| PlayerError::unavailable(&id))?;
            self.require_capability(
                state.transport_available()
                    && (state
                        .playback
                        .as_ref()
                        .is_some_and(|p| p.capabilities.pause)
                        || resuming
                            && self
                                .remote
                                .restored
                                .as_ref()
                                .is_some_and(|s| s.current.capabilities.pause)),
            )?;
            let observation_id = state.observation_id;
            let resume_device = self
                .remote
                .resume_pending
                .as_ref()
                .map(|pending| pending.device_id.clone());
            let owned = resume_device.is_none() && state.owned_output_control;
            let target_device = if owned {
                state
                    .playback
                    .as_ref()
                    .and_then(|p| p.device.as_ref())
                    .map(|d| d.id.clone())
            } else {
                resume_device.clone()
            };
            self.submit_remote(
                &id,
                RemoteAction::Control {
                    control: resume_device.as_ref().map_or(RemoteControl::Pause, |id| {
                        RemoteControl::PauseOnDevice {
                            device_id: id.clone(),
                        }
                    }),
                },
            )?;
            self.remote.resume_pending = None;
            self.remote.pending_local = Some(LocalSwitch {
                command: command.clone(),
                provider: id,
                pause: Some(PauseConfirmation {
                    observation_id,
                    target_device,
                    owned,
                }),
                deadline: Instant::now() + Duration::from_secs(10),
            });
            Ok(())
        })();
        Some(result)
    }
    pub(super) fn cancel_pending_remote_switch(&mut self) {
        self.remote.pending_local = None;
    }
    pub(super) fn has_pending_local_switch(&self) -> bool {
        self.remote.pending_local.is_some()
    }
    fn finish_local_switch(&mut self) {
        let Some(pending) = self.remote.pending_local.as_ref() else {
            return;
        };
        let Some(pause) = pending.pause.as_ref() else {
            let starting = self
                .remote
                .providers
                .get(&pending.provider)
                .is_some_and(|p| p.has_pending_output_start());
            if starting && Instant::now() < pending.deadline {
                return;
            }
            let pending = self.remote.pending_local.take().unwrap();
            let result = if starting {
                Err(PlayerError::new(
                    ErrorCode::ProviderUnavailable,
                    "Remote playback did not settle. Local audio was not started.",
                ))
            } else {
                // Recheck the now-current frame and, if needed, request Pause.
                self.execute(pending.command, None)
            };
            if let Err(error) = result {
                self.publish(PlayerEvent::Error { error });
            }
            self.publish_snapshot();
            return;
        };
        let state = self.remote.states.get(&pending.provider);
        let confirmed = state.is_some_and(|s| {
            if s.observation_id == pause.observation_id {
                return false;
            }
            if pause.owned {
                return s.owned_output_observed
                    && s.playback.as_ref().is_some_and(|p| {
                        p.state == PlaybackState::Paused
                            && p.device.as_ref().map(|d| &d.id) == pause.target_device.as_ref()
                    });
            }
            s.connection == ProviderConnectionState::Connected
                && s.playback.as_ref().is_none_or(|p| {
                    !matches!(p.state, PlaybackState::Playing | PlaybackState::Loading)
                        || pause
                            .target_device
                            .as_ref()
                            .is_some_and(|id| p.device.as_ref().is_none_or(|d| &d.id != id))
                })
        });
        let failed = state
            .filter(|s| !pause.owned || !s.owned_output_observed)
            .and_then(|s| s.issue.as_ref())
            .cloned();
        if !confirmed && failed.is_none() && Instant::now() < pending.deadline {
            return;
        }
        let pending = self.remote.pending_local.take().unwrap();
        let result = if confirmed {
            self.remote.restored = None;
            self.remote.resume_pending = None;
            self.remote.candidate = None;
            self.remote.last_selection = None;
            self.remote.active = None;
            self.execute(pending.command, None)
        } else {
            self.remote.restored = None;
            self.remote.resume_pending = None;
            Err(failed.unwrap_or_else(|| {
                PlayerError::new(
                    ErrorCode::ProviderUnavailable,
                    "Remote playback did not confirm Pause. Local audio was not started.",
                )
            }))
        };
        if let Err(error) = result {
            self.publish(PlayerEvent::Error { error });
        }
        self.publish_snapshot();
    }
    pub(super) fn remote_snapshot(&self) -> Option<PlayerSnapshot> {
        if let Some(snapshot) = self.restored_remote_snapshot() {
            return Some(snapshot);
        }
        let id = self.remote.active.as_ref()?;
        let state = self.remote.states.get(id);
        let frame = state.and_then(|s| s.playback.as_ref());
        let connected = state.is_some_and(|s| s.connection == ProviderConnectionState::Connected);
        let observed = state
            .is_some_and(|s| !s.authentication_required && (connected || s.owned_output_observed));
        let transport_available = state.is_some_and(RemoteState::transport_available);
        Some(PlayerSnapshot {
            host_analysis_status: self
                .host_visuals_are_current()
                .then(|| {
                    self.host_analysis
                        .source
                        .as_ref()
                        .map(|source| source.status())
                })
                .flatten(),
            host_spatial_status: self.host_spatial_status(),
            host_analysis_revoked_through: self.host_analysis_revoked_through(),
            enhancement: frame.and_then(|p| p.enhancement),
            restored_remote: None,
            sound: self.sound.clone(),
            output_statistics: None,
            revision: self.revision,
            // Connection health does not imply the remote device actually paused.
            state: frame.map_or(PlaybackState::Idle, |p| p.state),
            playback_rate: frame.map_or(1.0, |p| p.position_rate),
            position_ms: frame.map_or(0, |p| {
                let elapsed = if observed && p.state == PlaybackState::Playing {
                    self.remote.anchors.get(id).map_or(0, |(_, anchor)| {
                        (anchor.elapsed().as_secs_f64() * 1000.0 * p.position_rate)
                            .min(u64::MAX as f64) as u64
                    })
                } else {
                    0
                };
                p.position_ms
                    .saturating_add(elapsed)
                    .min(p.current.as_ref().map_or(0, |i| i.duration_ms))
            }),
            volume: frame.map_or(0.0, |p| p.volume),
            active_provider: Some(id.clone()),
            provider_connection: state
                .map_or(ProviderConnectionState::Connecting, |s| s.connection),
            capabilities: {
                let mut capabilities = frame
                    .filter(|_| transport_available)
                    .map_or(Capabilities::default(), |p| p.capabilities);
                capabilities.pcm_analysis = self.host_visuals_are_current()
                    && self
                        .host_analysis
                        .source
                        .as_ref()
                        .and_then(|source| source.analysis())
                        .is_some();
                capabilities
            },
            transport: frame
                .filter(|_| transport_available)
                .map_or(TransportControls::default(), |p| p.transport),
            playback_session_active: observed
                && frame.is_some_and(|p| {
                    p.current.is_some()
                        && matches!(p.state, PlaybackState::Playing | PlaybackState::Paused)
                }),
            queue: QueueSnapshot {
                revision: self.revision,
                context: PlaybackContext::UserQueue,
                current: frame.and_then(|p| p.current.clone()),
                upcoming: frame.map_or_else(Vec::new, |p| p.upcoming.clone()),
                history: vec![],
                device_queue: frame.and_then(|p| p.device_queue.clone()),
                repeat: frame.map_or(RepeatMode::Off, |p| p.repeat),
                shuffle: frame.is_some_and(|p| p.shuffle),
                play_next_count: 0,
                forward_count: 0,
            },
            session: self.session_state.clone(),
            queue_issues: vec![],
        })
    }
    pub(super) fn poll_remote(&mut self) {
        let ids: Vec<_> = self.remote.providers.keys().cloned().collect();
        self.poll_remote_updates(ids);
    }
    pub(super) fn poll_owned_request(&mut self, ready: impl FnOnce(&dyn RemoteProvider) -> bool) {
        if let Some(id) = &self.remote.active
            && self
                .remote
                .providers
                .get(id)
                .is_some_and(|provider| ready(provider.as_ref()))
        {
            self.poll_remote_updates([id.clone()]);
        }
    }
    pub(super) fn may_confirm_owned_handoff(&self, command: &PlayerCommand) -> bool {
        match command {
            PlayerCommand::Previous
            | PlayerCommand::SkipPrevious
            | PlayerCommand::Next
            | PlayerCommand::Pause => true,
            PlayerCommand::Remote { provider, action } => {
                self.remote.active.as_ref() == Some(provider)
                    && matches!(
                        action,
                        RemoteAction::OutputClock { clock: Some(_) }
                            | RemoteAction::OutputQueue { .. }
                            | RemoteAction::OutputCommandResult {
                                result: RemoteOutputCommandResult { accepted: true, .. }
                            }
                            | RemoteAction::Control {
                                control: RemoteControl::Previous
                                    | RemoteControl::Next
                                    | RemoteControl::Pause
                                    | RemoteControl::PauseOnDevice { .. }
                            }
                    )
            }
            _ => false,
        }
    }
    pub(super) fn poll_owned_handoff(&mut self) {
        if let Some(id) = &self.remote.active
            && self
                .remote
                .providers
                .get(id)
                .is_some_and(|provider| provider.owned_handoff_ready())
        {
            // An exact receipt, clock or queue can open the handoff after the
            // coordinator's pre-request poll. Drain it before the idle tick.
            self.poll_remote_updates([id.clone()]);
        }
    }
    fn poll_remote_updates(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            for _ in 0..4 {
                let Some(update) = self.remote.providers.get_mut(&id).and_then(|p| p.poll()) else {
                    break;
                };
                match update {
                    RemoteUpdate::State(state) => {
                        let state = *state;
                        if state.authentication_required {
                            self.remote.accounts.remove(&id);
                            self.hide_restored_remote(&id);
                        }
                        if state.playback.is_none() {
                            self.remote.anchors.remove(&id);
                        } else if self
                            .remote
                            .anchors
                            .get(&id)
                            .is_none_or(|(observation, _)| *observation != state.observation_id)
                        {
                            self.remote
                                .anchors
                                .insert(id.clone(), (state.observation_id, Instant::now()));
                        }
                        self.remote.states.insert(id.clone(), state.clone());
                        self.confirm_remote_resume(&id, &state);
                        self.capture_remote_selection(&id, &state);
                        self.observe_owned_pause(&id, &state);
                        self.observe_owned_recovery_state(&id, &state);
                        self.publish(PlayerEvent::RemoteState {
                            state: Box::new(state),
                        });
                        if self.remote.active.as_deref() == Some(&id) {
                            self.publish_snapshot();
                        }
                    }
                    RemoteUpdate::Edited(result) => {
                        self.publish(PlayerEvent::RemoteEdited { result })
                    }
                    RemoteUpdate::OutputCommand(command) => {
                        self.publish(PlayerEvent::RemoteOutputCommand {
                            provider: id.clone(),
                            command,
                        })
                    }
                    RemoteUpdate::OutputCommandFailed { control, error } => {
                        if self.remote.active.as_deref() == Some(&id) {
                            self.fail_host_spatial_relative(&control);
                        }
                        if control == RemoteOutputControl::Next
                            && self.remote.active.as_deref() == Some(&id)
                        {
                            self.cancel_host_spatial_next();
                        }
                        if control == RemoteOutputControl::Pause
                            && self.remote.pending_local.as_ref().is_some_and(|pending| {
                                pending.provider == id
                                    && pending.pause.as_ref().is_some_and(|pause| pause.owned)
                            })
                        {
                            self.remote.pending_local = None;
                        }
                        self.publish(PlayerEvent::Error { error });
                    }
                    RemoteUpdate::Page(page) => {
                        if page.provider == id {
                            self.accept_remote_account_page(&page);
                        }
                        self.update_restored_metadata(&page);
                        self.publish(PlayerEvent::RemotePage { page });
                    }
                }
            }
        }
        self.finish_local_switch();
    }
    pub(super) fn route_remote(
        &mut self,
        command: &PlayerCommand,
        request_id: Option<u64>,
    ) -> Option<Result<(), PlayerError>> {
        if let PlayerCommand::Remote { provider, action } = command {
            let directed_pause = if matches!(
                action,
                RemoteAction::Control {
                    control: RemoteControl::Pause
                }
            ) && self.remote_selection_needs_resume()
            {
                let Some(pending) = &self.remote.resume_pending else {
                    return Some(Ok(()));
                };
                Some(RemoteAction::Control {
                    control: RemoteControl::PauseOnDevice {
                        device_id: pending.device_id.clone(),
                    },
                })
            } else {
                None
            };
            let action = directed_pause.as_ref().unwrap_or(action);
            let result = (|| {
                if provider.is_empty() || provider.len() > 64 {
                    return Err(PlayerError::invalid("Invalid remote provider"));
                }
                if let RemoteAction::Control {
                    control:
                        RemoteControl::StartSpokenOnDevice { item, .. }
                        | RemoteControl::StartContext {
                            item: Some(item), ..
                        },
                } = action
                    && item.id.provider() != provider
                {
                    return Err(PlayerError::invalid(
                        "Playback item belongs to another provider",
                    ));
                }
                self.validate_remote_resume(provider, action)?;
                if matches!(
                    action,
                    RemoteAction::Control {
                        control: RemoteControl::Enqueue { .. }
                    }
                ) {
                    self.require_capability(self.remote.states.get(provider).is_some_and(|s| {
                        s.transport_available()
                            && s.playback.as_ref().is_some_and(|p| p.transport.queue_add)
                    }))?;
                }
                let activates = matches!(
                    action,
                    RemoteAction::Activate
                        | RemoteAction::Control {
                            control: RemoteControl::Start { .. }
                                | RemoteControl::StartOnDevice { .. }
                                | RemoteControl::StartSpokenOnDevice { .. }
                                | RemoteControl::ResumeOnDevice { .. }
                                | RemoteControl::StartContext { .. }
                                | RemoteControl::Transfer { .. }
                        }
                );
                if let (
                    RemoteAction::Control {
                        control: RemoteControl::PauseOnDevice { device_id },
                    },
                    Some(request_id),
                ) = (action, request_id)
                {
                    self.remote
                        .providers
                        .get_mut(provider)
                        .ok_or_else(|| PlayerError::unavailable(provider))?
                        .submit_pause_on_device_with_request_id(device_id, request_id)?;
                } else {
                    self.submit_remote(provider, action.clone())?;
                }
                self.note_owned_recovery_clock(provider, action);
                self.note_remote_action(provider, action);
                if activates {
                    self.activate_remote(provider)?;
                    if let Some(state) = self.remote.states.get(provider).cloned() {
                        self.capture_remote_selection(provider, &state);
                    }
                }
                if matches!(action, RemoteAction::Disconnect) {
                    self.remote.states.remove(provider);
                    if self.remote.active.as_deref() == Some(provider) {
                        self.remote.active = None;
                        self.active = self.queue.current().map(|i| i.id.provider().to_owned());
                        self.state = if self.active.is_some() {
                            PlaybackState::Paused
                        } else {
                            PlaybackState::Idle
                        };
                    }
                }
                Ok(())
            })();
            return Some(result);
        }
        let id = self.remote.active.clone()?;
        let state = self.snapshot();
        if self.remote_selection_needs_resume() {
            if matches!(command, PlayerCommand::Pause) && self.remote.resume_pending.is_some() {
                let device_id = self
                    .remote
                    .resume_pending
                    .as_ref()
                    .unwrap()
                    .device_id
                    .clone();
                let action = RemoteAction::Control {
                    control: RemoteControl::PauseOnDevice { device_id },
                };
                return Some(self.submit_remote(&id, action.clone()).map(|()| {
                    self.note_remote_action(&id, &action);
                }));
            }
            if matches!(command, PlayerCommand::Play) {
                return Some(Err(PlayerError::invalid(
                    "Resume this saved selection on an explicit playback device",
                )));
            }
        }
        let owned_device = self
            .remote
            .states
            .get(&id)
            .filter(|remote| remote.owned_output_control && remote.transport_available())
            .and_then(|remote| remote.playback.as_ref()?.device.as_ref())
            .filter(|device| device.is_active && !device.is_restricted);
        let owned_current = owned_device.and(state.queue.current.as_ref());
        if matches!(command, PlayerCommand::SkipPrevious)
            && !state.transport.previous
            && state.restored_remote.is_none()
            && let Some(device) = owned_device
        {
            let device_id = device.id.clone();
            return Some(
                self.remote
                    .providers
                    .get_mut(&id)
                    .ok_or_else(|| PlayerError::unavailable(&id))
                    .and_then(|provider| provider.submit_previous_after_next(&device_id)),
            );
        }
        let (control, allowed) = match command {
            PlayerCommand::Play => (
                RemoteControl::Play,
                state.capabilities.play || owned_current.is_some_and(|item| item.capabilities.play),
            ),
            PlayerCommand::Pause => (
                RemoteControl::Pause,
                state.capabilities.pause
                    || owned_current.is_some_and(|item| item.capabilities.pause),
            ),
            PlayerCommand::Stop => (RemoteControl::Stop, state.transport.stop),
            PlayerCommand::Next => (RemoteControl::Next, state.transport.next),
            PlayerCommand::Previous if state.position_ms >= PREVIOUS_RESTART_MS => (
                RemoteControl::Seek { position_ms: 0 },
                state.capabilities.seek,
            ),
            PlayerCommand::Previous | PlayerCommand::SkipPrevious => {
                (RemoteControl::Previous, state.transport.previous)
            }
            PlayerCommand::Seek { position_ms } => (
                RemoteControl::Seek {
                    position_ms: (*position_ms)
                        .min(state.queue.current.as_ref().map_or(0, |i| i.duration_ms)),
                },
                state.capabilities.seek,
            ),
            PlayerCommand::SetVolume { volume } => {
                if !volume.is_finite() || !(0.0..=1.0).contains(volume) {
                    return Some(Err(PlayerError::invalid("Volume must be between 0 and 1")));
                }
                (
                    RemoteControl::Volume { volume: *volume },
                    state.capabilities.volume,
                )
            }
            PlayerCommand::SetShuffle { enabled } => (
                RemoteControl::Shuffle { enabled: *enabled },
                state.transport.shuffle,
            ),
            PlayerCommand::SetRepeat { mode } => (
                RemoteControl::Repeat { mode: *mode },
                state.transport.repeat,
            ),
            PlayerCommand::PlayNext { item } => (
                RemoteControl::Enqueue { item: item.clone() },
                state.transport.queue_add,
            ),
            PlayerCommand::JumpQueue { .. }
            | PlayerCommand::RemoveQueueItem { .. }
            | PlayerCommand::ClearQueue => {
                return Some(Err(PlayerError::new(
                    ErrorCode::UnsupportedCapability,
                    "This source manages its queue on the playback device",
                )));
            }
            _ => return None,
        };
        let can_bind_owned_request = owned_current.is_some() && state.restored_remote.is_none();
        let action = RemoteAction::Control { control };
        Some(self.require_capability(allowed).and_then(|()| {
            if let (PlayerCommand::Next, Some(request_id)) = (command, request_id) {
                self.remote
                    .providers
                    .get_mut(&id)
                    .ok_or_else(|| PlayerError::unavailable(&id))?
                    .submit_next_with_request_id(request_id)?;
            } else if let (
                RemoteAction::Control {
                    control: RemoteControl::Previous,
                },
                Some(request_id),
            ) = (&action, request_id)
                && can_bind_owned_request
            {
                self.remote
                    .providers
                    .get_mut(&id)
                    .ok_or_else(|| PlayerError::unavailable(&id))?
                    .submit_previous_with_request_id(request_id)?;
            } else if let (
                RemoteAction::Control {
                    control: RemoteControl::Seek { position_ms },
                },
                Some(request_id),
            ) = (&action, request_id)
                && can_bind_owned_request
            {
                self.remote
                    .providers
                    .get_mut(&id)
                    .ok_or_else(|| PlayerError::unavailable(&id))?
                    .submit_seek_with_request_id(*position_ms, request_id)?;
            } else if let (PlayerCommand::Play | PlayerCommand::Pause, Some(request_id)) =
                (command, request_id)
                && can_bind_owned_request
            {
                self.remote
                    .providers
                    .get_mut(&id)
                    .ok_or_else(|| PlayerError::unavailable(&id))?
                    .submit_play_pause_with_request_id(
                        matches!(command, PlayerCommand::Play),
                        request_id,
                    )?;
            } else {
                self.submit_remote(&id, action.clone())?;
            }
            self.note_owned_pause_action(&id, &action);
            Ok(())
        }))
    }
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
