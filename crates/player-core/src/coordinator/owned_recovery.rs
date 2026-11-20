//! Inert selection recovery after a confirmed unexpected owned-child exit.
//! A missing clock never proves Pause and never promotes this checkpoint.
use super::*;
use std::time::Instant;

const CHECKPOINT_FRESHNESS: Duration = Duration::from_secs(2);
const CLOCK_FRESHNESS: Duration = Duration::from_millis(900);

#[derive(Default)]
pub(super) struct OwnedOutputRecovery {
    clock: Option<RecoveryClock>,
    checkpoint: Option<Checkpoint>,
    fence: Option<SelectionFence>,
    watermark: (u64, u64),
    interrupted: Option<InterruptedSelection>,
    seek: Option<(u64, u64)>,
    awaiting_account: bool,
    last_drop: Option<(&'static str, Instant)>,
    state_gate: &'static str,
    pause_gate: &'static str,
    last_traced_exit: Option<(u64, u64)>,
}

struct RecoveryClock {
    provider: String,
    value: RemoteOutputClock,
    observed: Instant,
}

struct Checkpoint {
    selection: RemoteSelection,
    device: String,
    generation: u64,
    lineage_sequence: u64,
    seen_sequence: u64,
    uptime: u64,
    epoch: Option<u64>,
    state: PlaybackState,
    observed: Instant,
}

struct SelectionFence {
    generation: u64,
    device: Option<String>,
    media: Option<MediaId>,
    context: Option<String>,
    epoch: Option<u64>,
}

struct InterruptedSelection {
    provider: String,
    device: String,
    generation: u64,
}

fn context(clock: &RemoteOutputClock) -> Option<&str> {
    clock.metadata.as_ref()?.context_uri.as_deref()
}

impl SelectionFence {
    fn permits(&self, clock: &RemoteOutputClock) -> bool {
        clock.generation > self.generation
            || self.device.as_ref().is_some_and(|d| d != &clock.device_id)
            || self.media.is_some() && self.media != clock.media_id
            || clock.metadata.is_some() && self.context.as_deref() != context(clock)
            || self
                .epoch
                .zip(clock.queue_epoch)
                .is_some_and(|(old, new)| new > old)
    }
}

impl OwnedOutputRecovery {
    fn drop_checkpoint(&mut self, reason: &'static str) {
        if self.checkpoint.take().is_some() {
            self.last_drop = Some((reason, Instant::now()));
        }
    }

    fn cancel_provenance(&mut self, reason: &'static str) {
        self.fence = self
            .clock
            .as_ref()
            .map(|clock| SelectionFence {
                generation: clock.value.generation,
                device: Some(clock.value.device_id.clone()),
                media: clock.value.media_id.clone(),
                context: context(&clock.value).map(str::to_owned),
                epoch: clock.value.queue_epoch,
            })
            .or_else(|| {
                self.checkpoint.as_ref().map(|saved| SelectionFence {
                    generation: saved.generation,
                    device: Some(saved.device.clone()),
                    media: Some(saved.selection.current.id.clone()),
                    context: saved.selection.context_uri.clone(),
                    epoch: saved.epoch,
                })
            })
            .or_else(|| {
                (self.watermark.0 > 0).then_some(SelectionFence {
                    generation: self.watermark.0,
                    device: None,
                    media: None,
                    context: None,
                    epoch: None,
                })
            });
        self.clock = None;
        self.drop_checkpoint(reason);
        self.seek = None;
    }

    pub(super) fn resumed(&mut self) {
        self.interrupted = None;
        self.fence = None;
    }

    pub(super) fn clear_interrupted(&mut self) {
        self.interrupted = None;
    }

    pub(super) fn account_verified(&mut self) {
        if self.awaiting_account {
            // Verification allows a new full actual Playing observation to
            // seed again; it never restores the discarded pre-credential proof.
            self.awaiting_account = false;
            self.fence = None;
        }
    }

    #[cfg(test)]
    pub(super) fn expire_checkpoint(&mut self) {
        if let Some(saved) = &mut self.checkpoint {
            saved.observed -= CHECKPOINT_FRESHNESS + Duration::from_millis(1);
        }
    }
}

impl PlaybackCoordinator {
    pub(super) fn reconcile_owned_recovery_account(&mut self, provider: &str) {
        if self
            .remote
            .owned_recovery
            .checkpoint
            .as_ref()
            .is_some_and(|saved| {
                saved.selection.provider == provider
                    && !self.is_remote_selection_verified(&saved.selection)
            })
        {
            self.remote
                .owned_recovery
                .cancel_provenance("account_unverified");
        }
    }

    pub(super) fn interrupted_owned_device(&self) -> Option<&str> {
        let interrupted = self.remote.owned_recovery.interrupted.as_ref()?;
        self.remote
            .restored
            .as_ref()
            .filter(|saved| saved.provider == interrupted.provider)?;
        Some(&interrupted.device)
    }

    pub(super) fn cancel_owned_output_recovery(
        &mut self,
        provider: &str,
    ) -> Result<(), PlayerError> {
        if provider.is_empty() || provider.len() > 64 {
            return Err(PlayerError::invalid("Invalid remote provider"));
        }
        if self.remote.active.as_deref() == Some(provider) {
            // This command cancels only recovery evidence. In particular, an
            // explicit engine retry does not discard an already saved selection.
            self.remote
                .owned_recovery
                .cancel_provenance("explicit_reset");
            self.remote.owned_recovery.awaiting_account = false;
        }
        Ok(())
    }

    pub(super) fn note_owned_recovery_intent(&mut self, command: &PlayerCommand) {
        let selected = match command {
            PlayerCommand::OpenFile { .. }
            | PlayerCommand::ReplaceQueue { .. }
            | PlayerCommand::PlayLibrary { .. }
            | PlayerCommand::Stop
            | PlayerCommand::Next
            | PlayerCommand::Previous
            | PlayerCommand::SkipPrevious
            | PlayerCommand::JumpQueue { .. }
            | PlayerCommand::ClearQueue => self.remote.active.as_deref(),
            PlayerCommand::Remote {
                provider,
                action:
                    RemoteAction::Credential { .. }
                    | RemoteAction::Disconnect
                    | RemoteAction::Activate
                    | RemoteAction::Control {
                        control:
                            RemoteControl::Stop
                            | RemoteControl::Next
                            | RemoteControl::Previous
                            | RemoteControl::Transfer { .. }
                            | RemoteControl::Start { .. }
                            | RemoteControl::StartOnDevice { .. }
                            | RemoteControl::StartSpokenOnDevice { .. }
                            | RemoteControl::StartContext { .. }
                            | RemoteControl::ResumeOnDevice { .. },
                    },
            } => Some(provider.as_str()),
            _ => None,
        };
        if selected.is_some() && selected == self.remote.active.as_deref() {
            self.remote
                .owned_recovery
                .cancel_provenance("selection_intent");
            self.remote.owned_recovery.awaiting_account = matches!(
                command,
                PlayerCommand::Remote {
                    action: RemoteAction::Credential { .. },
                    ..
                }
            );
        }
        let seek = match command {
            PlayerCommand::Seek { position_ms } => Some(*position_ms),
            PlayerCommand::Remote {
                provider,
                action:
                    RemoteAction::Control {
                        control: RemoteControl::Seek { position_ms },
                    },
            } if self.remote.active.as_ref() == Some(provider) => Some(*position_ms),
            _ => None,
        };
        if let Some(position) = seek {
            let recovery = &mut self.remote.owned_recovery;
            recovery.seek = Some((position, recovery.watermark.1));
        }
    }

    fn freeze_owned_checkpoint_position(&mut self) {
        if self.remote.owned_recovery.seek.is_some() {
            return;
        }
        let Some(saved) = self.remote.owned_recovery.checkpoint.as_ref() else {
            return;
        };
        let Some(clock) = self.remote.owned_recovery.clock.as_ref() else {
            return;
        };
        if clock.value.generation != saved.generation
            || clock.value.state != saved.state
            || !self
                .remote
                .states
                .get(&saved.selection.provider)
                .is_some_and(|state| {
                    state.owned_output_observed
                        && state.playback.as_ref().is_some_and(|frame| {
                            frame.state == saved.state
                                && frame.position_ms.abs_diff(clock.value.position_ms) <= 250
                                && frame.context_uri == saved.selection.context_uri
                                && frame
                                    .current
                                    .as_ref()
                                    .is_some_and(|item| item.id == saved.selection.current.id)
                        })
                })
        {
            return;
        }
        let Some(selected) = self.remote_selection_for_storage() else {
            return;
        };
        let Some(saved) = self.remote.owned_recovery.checkpoint.as_mut() else {
            return;
        };
        // Playing may advance between native samples. A confirmed Pause has
        // already stored its raw anchor and must not reimport UI extrapolation.
        if saved.state == PlaybackState::Playing
            && saved.observed.elapsed() <= CHECKPOINT_FRESHNESS
            && saved.selection.current.id == selected.current.id
            && saved.selection.context_uri == selected.context_uri
            && saved.selection.provider == selected.provider
            && saved.selection.account_id == selected.account_id
            && saved.selection.stable_account_id == selected.stable_account_id
        {
            saved.selection.position_ms = selected.position_ms;
        }
    }

    pub(super) fn note_owned_recovery_clock(&mut self, provider: &str, action: &RemoteAction) {
        if self.remote.active.as_deref() != Some(provider) {
            return;
        }
        let RemoteAction::OutputClock { clock } = action else {
            return;
        };
        let Some(clock) = clock else {
            self.freeze_owned_checkpoint_position();
            self.remote.owned_recovery.clock = None;
            return;
        };
        let recovery = &mut self.remote.owned_recovery;
        if (clock.generation, clock.sequence) <= recovery.watermark {
            return;
        }
        recovery.watermark = (clock.generation, clock.sequence);
        let valid = clock.generation > 0
            && clock.sequence > 0
            && !clock.device_id.is_empty()
            && clock.device_id.len() <= 256
            && clock.control_uptime_ms.is_some()
            && clock.rate.is_finite()
            && (0.0..=4.0).contains(&clock.rate);
        if !valid {
            recovery.clock = None;
            recovery.drop_checkpoint(if clock.control_uptime_ms.is_none() {
                "clock_no_uptime"
            } else {
                "clock_invalid"
            });
            return;
        }
        if recovery.clock.as_ref().is_some_and(|old| {
            old.value.generation == clock.generation
                && old
                    .value
                    .control_uptime_ms
                    .zip(clock.control_uptime_ms)
                    .is_some_and(|(old, new)| new < old)
        }) || recovery.checkpoint.as_ref().is_some_and(|saved| {
            saved.generation == clock.generation
                && clock.control_uptime_ms.is_some_and(|uptime| {
                    uptime < saved.uptime
                        || saved.observed.elapsed() <= CHECKPOINT_FRESHNESS
                            && uptime - saved.uptime
                                > saved.observed.elapsed().as_millis() as u64
                                    + CLOCK_FRESHNESS.as_millis() as u64
                })
        }) {
            recovery.clock = None;
            recovery.drop_checkpoint("clock_uptime_jump");
            return;
        }
        if let Some(saved) = &mut recovery.checkpoint {
            let same = saved.generation == clock.generation
                && saved.device == clock.device_id
                && self.remote.providers.get(provider).is_some_and(|source| {
                    clock.media_id.as_ref().is_some_and(|media| {
                        source.output_media_matches(media, &saved.selection.current.id)
                    })
                })
                && clock
                    .metadata
                    .as_ref()
                    .is_none_or(|_| context(clock) == saved.selection.context_uri.as_deref())
                && clock
                    .control_uptime_ms
                    .is_some_and(|uptime| uptime >= saved.uptime)
                && matches!(clock.state, PlaybackState::Playing | PlaybackState::Paused)
                && saved.observed.elapsed() <= CHECKPOINT_FRESHNESS;
            if !same {
                let reason = if saved.observed.elapsed() > CHECKPOINT_FRESHNESS {
                    "full_proof_expired"
                } else if saved.generation != clock.generation {
                    "runtime_scope"
                } else if saved.device != clock.device_id {
                    "device_scope"
                } else if clock
                    .metadata
                    .as_ref()
                    .is_some_and(|_| context(clock) != saved.selection.context_uri.as_deref())
                {
                    "context_scope"
                } else if !matches!(clock.state, PlaybackState::Playing | PlaybackState::Paused) {
                    "clock_state"
                } else {
                    "media_or_uptime_scope"
                };
                recovery.drop_checkpoint(reason);
            } else {
                saved.seen_sequence = clock.sequence;
                if clock.state == PlaybackState::Paused {
                    recovery.pause_gate = if clock.actions.is_none() {
                        "no_actions"
                    } else if clock.metadata.is_none() {
                        "no_metadata"
                    } else if clock.position_ms > saved.selection.current.duration_ms {
                        "position_out_of_bounds"
                    } else {
                        "position_unproven"
                    };
                }
                if clock.state == PlaybackState::Paused
                    && clock.rate == 0.0
                    && clock.actions.is_some()
                    && clock.metadata.is_some()
                    && clock.position_ms <= saved.selection.current.duration_ms
                {
                    let confirmed_seek = recovery.seek.is_some_and(|(position, after)| {
                        clock.sequence > after && clock.position_ms.abs_diff(position) <= 250
                    });
                    let plausible = recovery.seek.is_none()
                        && (saved.selection.position_ms.abs_diff(clock.position_ms) <= 250
                            || saved.state == PlaybackState::Playing
                                && clock.position_ms
                                    >= saved.selection.position_ms.saturating_sub(250)
                                && clock.position_ms
                                    <= saved.selection.position_ms.saturating_add(
                                        saved.observed.elapsed().as_millis() as u64 * 4 + 1_000,
                                    ));
                    if plausible || confirmed_seek {
                        recovery.pause_gate = "refreshed";
                        saved.selection.position_ms = clock.position_ms;
                        saved.uptime = clock.control_uptime_ms.unwrap();
                        saved.epoch = clock.queue_epoch;
                        saved.state = PlaybackState::Paused;
                        saved.observed = Instant::now();
                        if confirmed_seek {
                            recovery.seek = None;
                        }
                    }
                }
            }
        }
        recovery.clock = Some(RecoveryClock {
            provider: provider.into(),
            value: clock.clone(),
            observed: Instant::now(),
        });
    }

    pub(super) fn observe_owned_recovery_state(&mut self, provider: &str, state: &RemoteState) {
        if self.remote.active.as_deref() != Some(provider) {
            return;
        }
        self.remote.owned_recovery.state_gate = "state_seen";
        let device = self
            .remote
            .owned_recovery
            .checkpoint
            .as_ref()
            .map(|s| s.device.as_str())
            .or_else(|| {
                self.remote
                    .owned_recovery
                    .interrupted
                    .as_ref()
                    .map(|s| s.device.as_str())
            });
        let foreign = device.is_some_and(|owned| {
            state.devices.iter().any(|d| d.is_active && d.id != owned)
                || state.playback.as_ref().is_some_and(|frame| {
                    frame.current.is_some() && frame.device.as_ref().is_some_and(|d| d.id != owned)
                })
        });
        if state.authentication_required
            || matches!(
                state.connection,
                ProviderConnectionState::Disconnected | ProviderConnectionState::Connecting
            )
            || foreign
        {
            let reason = if state.authentication_required {
                "state_auth"
            } else if foreign {
                "state_foreign"
            } else {
                "state_connection"
            };
            self.remote.owned_recovery.cancel_provenance(reason);
            if foreign && self.remote.owned_recovery.interrupted.take().is_some() {
                self.remote.restored = None;
                self.remote.resume_pending = None;
                self.session_state.restored = false;
                self.session_dirty = true;
            }
            return;
        }
        if self
            .remote
            .owned_recovery
            .checkpoint
            .as_ref()
            .is_some_and(|saved| {
                !self.is_remote_selection_verified(&saved.selection)
                    || state.playback.as_ref().is_some_and(|frame| {
                        frame
                            .current
                            .as_ref()
                            .is_some_and(|item| item.id != saved.selection.current.id)
                            || frame.context_uri.as_ref().is_some_and(|context| {
                                Some(context) != saved.selection.context_uri.as_ref()
                            })
                    })
            })
        {
            self.remote
                .owned_recovery
                .cancel_provenance("projected_selection");
        }
        let Some(clock) = self.remote.owned_recovery.clock.as_ref().filter(|c| {
            c.provider == provider
                && c.observed.elapsed() <= CLOCK_FRESHNESS
                && c.value.actions.is_some()
                && c.value.metadata.is_some()
                && c.value.active
                && (c.value.state == PlaybackState::Playing && c.value.rate > 0.0
                    || c.value.state == PlaybackState::Paused && c.value.rate == 0.0)
        }) else {
            self.remote.owned_recovery.state_gate = "no_full_fresh_clock";
            return;
        };
        let Some(frame) = state.playback.as_ref().filter(|frame| {
            state.owned_output_observed
                && frame.state == clock.value.state
                && frame.position_rate.is_finite()
                && (frame.state == PlaybackState::Playing && frame.position_rate > 0.0
                    || frame.state == PlaybackState::Paused && frame.position_rate == 0.0)
                && frame.position_rate <= 4.0
                && frame.position_ms.abs_diff(clock.value.position_ms) <= 250
                && frame.context_uri.as_deref() == context(&clock.value)
                && frame.device.as_ref().is_some_and(|d| {
                    d.id == clock.value.device_id && d.is_active && !d.is_restricted
                })
                && frame.current.as_ref().is_some_and(|item| {
                    self.remote.providers.get(provider).is_some_and(|source| {
                        clock
                            .value
                            .media_id
                            .as_ref()
                            .is_some_and(|media| source.output_media_matches(media, &item.id))
                    })
                })
        }) else {
            self.remote.owned_recovery.state_gate = "frame_unmatched";
            return;
        };
        if frame.state == PlaybackState::Paused {
            // The UI/session anchor may run ahead of the native clock during a
            // handoff. A complete applied Pause can correct that existing
            // played lineage; it cannot create or revive one on its own.
            let corroborated_pause = !self.remote.owned_recovery.awaiting_account
                && self.remote.owned_recovery.fence.is_none()
                && self.remote.owned_recovery.seek.is_none()
                && self.remote.restored.is_none()
                && self.remote.candidate.is_none()
                && self.remote.resume_pending.is_none()
                && self.remote.confirmed_resume_pause.is_none()
                && !self.has_pending_local_switch()
                && self
                    .remote
                    .providers
                    .get(provider)
                    .is_some_and(|source| !source.has_pending_output_start())
                && self
                    .remote
                    .owned_recovery
                    .checkpoint
                    .as_ref()
                    .is_some_and(|saved| {
                        saved.state == PlaybackState::Playing
                            && saved.observed.elapsed() <= CHECKPOINT_FRESHNESS
                            && saved.generation == clock.value.generation
                            && saved.device == clock.value.device_id
                            && self.is_remote_selection_verified(&saved.selection)
                            && frame.current.as_ref().is_some_and(|item| {
                                item.id == saved.selection.current.id
                                    && item.duration_ms == saved.selection.current.duration_ms
                            })
                            && frame.context_uri == saved.selection.context_uri
                            && clock
                                .value
                                .queue_epoch
                                .is_some_and(|epoch| epoch > 0 && saved.epoch == Some(epoch))
                            && clock.value.metadata.as_ref().is_some_and(|metadata| {
                                metadata.duration_ms == saved.selection.current.duration_ms
                            })
                            && clock.value.position_ms <= saved.selection.current.duration_ms
                    });
            if corroborated_pause {
                let position = clock.value.position_ms;
                let uptime = clock.value.control_uptime_ms.unwrap();
                let observed = clock.observed;
                let saved = self.remote.owned_recovery.checkpoint.as_mut().unwrap();
                saved.selection.position_ms = position;
                saved.state = PlaybackState::Paused;
                saved.uptime = uptime;
                saved.observed = observed;
                self.remote.owned_recovery.pause_gate = "corroborated_pause";
            }
            self.remote.owned_recovery.state_gate =
                if self.remote.owned_recovery.checkpoint.is_some() {
                    "paused_retained"
                } else {
                    "paused_no_seed"
                };
            // A new child's active, complete observation may reflect an explicit
            // selection made elsewhere and paused before Auvra saw Playing.
            // A same-item inactive/zero startup pause cannot erase saved progress.
            if self
                .remote
                .owned_recovery
                .interrupted
                .as_ref()
                .is_some_and(|i| clock.value.generation > i.generation)
                && self.remote.restored.as_ref().is_some_and(|saved| {
                    self.is_remote_selection_verified(saved)
                        && (frame
                            .current
                            .as_ref()
                            .is_some_and(|item| item.id != saved.current.id)
                            || frame.context_uri != saved.context_uri)
                })
            {
                self.remote.owned_recovery.clear_interrupted();
                self.remote.restored = None;
                self.remote.resume_pending = None;
                self.session_state.restored = false;
                self.session_dirty = true;
            }
            return;
        }
        if self
            .remote
            .owned_recovery
            .interrupted
            .as_ref()
            .is_some_and(|i| clock.value.generation > i.generation)
        {
            self.remote.owned_recovery.interrupted = None;
            self.remote.restored = None;
            self.remote.resume_pending = None;
            self.session_state.restored = false;
        }
        if self.remote.restored.is_some()
            || self.remote.candidate.is_some()
            || self
                .remote
                .owned_recovery
                .fence
                .as_ref()
                .is_some_and(|fence| !fence.permits(&clock.value))
        {
            self.remote.owned_recovery.state_gate = "seed_fenced_or_restored";
            return;
        }
        let Some(mut selected) = self.remote_selection_for_storage().filter(|selected| {
            selected.provider == provider
                && self.is_remote_selection_verified(selected)
                && frame
                    .current
                    .as_ref()
                    .is_some_and(|item| item.id == selected.current.id)
                && selected.context_uri == frame.context_uri
                && clock.value.position_ms <= selected.current.duration_ms
        }) else {
            self.remote.owned_recovery.state_gate = "selection_unverified";
            return;
        };
        selected
            .current
            .artwork
            .clone_from(&frame.current.as_ref().unwrap().artwork);
        // Recovery evidence follows the accepted native anchor, not a second
        // extrapolation of the presentation/session clock.
        selected.position_ms = clock.value.position_ms;
        let clock = self.remote.owned_recovery.clock.as_ref().unwrap();
        let lineage_sequence = self
            .remote
            .owned_recovery
            .checkpoint
            .as_ref()
            .filter(|saved| {
                saved.observed.elapsed() <= CHECKPOINT_FRESHNESS
                    && saved.generation == clock.value.generation
                    && saved.device == clock.value.device_id
                    && saved.selection.current.id == selected.current.id
                    && saved.selection.context_uri == selected.context_uri
                    && (clock.value.position_ms >= saved.selection.position_ms.saturating_sub(250)
                        || self.remote.owned_recovery.seek.is_some())
                    && clock.value.confirmed_same_media_next.is_none()
            })
            .map_or(clock.value.sequence, |saved| saved.lineage_sequence);
        self.remote.owned_recovery.state_gate = "seeded_playing";
        self.remote.owned_recovery.last_drop = None;
        self.remote.owned_recovery.checkpoint = Some(Checkpoint {
            selection: selected,
            device: clock.value.device_id.clone(),
            generation: clock.value.generation,
            lineage_sequence,
            seen_sequence: clock.value.sequence,
            uptime: clock.value.control_uptime_ms.unwrap(),
            epoch: clock.value.queue_epoch,
            state: PlaybackState::Playing,
            observed: clock.observed,
        });
        self.remote.owned_recovery.fence = None;
        self.remote.owned_recovery.seek = None;
    }

    fn trace_owned_interruption(
        &mut self,
        provider: &str,
        generation: u64,
        sequence: u64,
        device: &str,
        accepted: bool,
    ) {
        if std::env::var_os("AUVRA_AUDIO_TRACE").is_none_or(|value| value != "1")
            || self.remote.owned_recovery.last_traced_exit == Some((generation, sequence))
        {
            return;
        }
        self.remote.owned_recovery.last_traced_exit = Some((generation, sequence));
        let recovery = &self.remote.owned_recovery;
        let saved = recovery.checkpoint.as_ref();
        let clock = recovery.clock.as_ref();
        let state = self.remote.states.get(provider);
        let frame = state.and_then(|state| state.playback.as_ref());
        let scope = saved.is_some_and(|saved| {
            saved.selection.provider == provider
                && self.remote.active.as_deref() == Some(provider)
                && saved.generation == generation
                && saved.device == device
        });
        let sequence_matches = saved.is_some_and(|saved| {
            sequence >= saved.lineage_sequence && sequence <= saved.seen_sequence
        });
        let fresh = saved.is_some_and(|saved| saved.observed.elapsed() <= CHECKPOINT_FRESHNESS);
        let account =
            saved.is_some_and(|saved| self.is_remote_selection_verified(&saved.selection));
        let auth = state.is_some_and(|state| !state.authentication_required);
        let conflict = state.is_some_and(|state| {
            state.devices.iter().any(|d| d.is_active && d.id != device)
                || state.playback.as_ref().is_some_and(|frame| {
                    frame.current.is_some() && frame.device.as_ref().is_some_and(|d| d.id != device)
                })
        });
        let reason = if accepted {
            "promoted"
        } else if saved.is_none() {
            "no_checkpoint"
        } else if !scope {
            "scope"
        } else if !sequence_matches {
            "sequence"
        } else if !fresh {
            "expired"
        } else if !account {
            "account"
        } else if self.remote.restored.is_some() || self.remote.candidate.is_some() {
            "restore_pending"
        } else if !auth || conflict {
            "auth_or_current_conflict"
        } else {
            "predicate_boundary"
        };
        eprintln!(
            "auvra_owned_recovery time_ms={} accepted={} reason={} incoming_generation={} incoming_sequence={} checkpoint={} last_drop={} drop_age_ms={} state_gate={} pause_gate={} saved_generation={} lineage_sequence={} seen_sequence={} saved_state={:?} saved_age_ms={} saved_epoch={:?} scope_match={} sequence_match={} fresh={} account_verified={} state_present={} auth_ok={} current_conflict={} restored={} candidate={} fence={} awaiting_account={} clock={} clock_age_ms={} clock_state={:?} clock_active={} clock_actions={} clock_metadata={} clock_uptime={} clock_generation={} clock_sequence={} clock_epoch={:?} raw_context_match={} frame={} frame_state={:?} frame_observed={} frame_context_match={} frame_media_match={}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            accepted,
            reason,
            generation,
            sequence,
            saved.is_some(),
            recovery.last_drop.map_or("none", |(reason, _)| reason),
            recovery
                .last_drop
                .map_or(0, |(_, at)| at.elapsed().as_millis()),
            recovery.state_gate,
            recovery.pause_gate,
            saved.map_or(0, |saved| saved.generation),
            saved.map_or(0, |saved| saved.lineage_sequence),
            saved.map_or(0, |saved| saved.seen_sequence),
            saved.map(|saved| saved.state),
            saved.map_or(0, |saved| saved.observed.elapsed().as_millis()),
            saved.and_then(|saved| saved.epoch),
            scope,
            sequence_matches,
            fresh,
            account,
            state.is_some(),
            auth,
            conflict,
            self.remote.restored.is_some(),
            self.remote.candidate.is_some(),
            recovery.fence.is_some(),
            recovery.awaiting_account,
            clock.is_some(),
            clock.map_or(0, |clock| clock.observed.elapsed().as_millis()),
            clock.map(|clock| clock.value.state),
            clock.is_some_and(|clock| clock.value.active),
            clock.is_some_and(|clock| clock.value.actions.is_some()),
            clock.is_some_and(|clock| clock.value.metadata.is_some()),
            clock.is_some_and(|clock| clock.value.control_uptime_ms.is_some()),
            clock.map_or(0, |clock| clock.value.generation),
            clock.map_or(0, |clock| clock.value.sequence),
            clock.and_then(|clock| clock.value.queue_epoch),
            clock
                .zip(saved)
                .is_some_and(|(clock, saved)| context(&clock.value)
                    == saved.selection.context_uri.as_deref()),
            frame.is_some(),
            frame.map(|frame| frame.state),
            state.is_some_and(|state| state.owned_output_observed),
            frame
                .zip(saved)
                .is_some_and(|(frame, saved)| frame.context_uri == saved.selection.context_uri),
            frame.zip(saved).is_some_and(|(frame, saved)| frame
                .current
                .as_ref()
                .is_some_and(|item| item.id == saved.selection.current.id)),
        );
    }

    pub(super) fn owned_output_interrupted(
        &mut self,
        provider: &str,
        generation: u64,
        sequence: u64,
        device: &str,
    ) -> Result<(), PlayerError> {
        if provider.is_empty()
            || provider.len() > 64
            || device.is_empty()
            || device.len() > 256
            || generation == 0
            || sequence == 0
        {
            return Err(PlayerError::invalid(
                "Invalid owned output interruption scope",
            ));
        }
        self.freeze_owned_checkpoint_position();
        let Some(saved) = self
            .remote
            .owned_recovery
            .checkpoint
            .as_ref()
            .filter(|saved| {
                saved.selection.provider == provider
                    && self.remote.active.as_deref() == Some(provider)
                    && saved.generation == generation
                    && saved.device == device
                    && sequence >= saved.lineage_sequence
                    && sequence <= saved.seen_sequence
                    && saved.observed.elapsed() <= CHECKPOINT_FRESHNESS
                    && self.is_remote_selection_verified(&saved.selection)
                    && self.remote.restored.is_none()
                    && self.remote.candidate.is_none()
                    && self.remote.states.get(provider).is_some_and(|state| {
                        !state.authentication_required
                            && !state.devices.iter().any(|d| d.is_active && d.id != device)
                            && state.playback.as_ref().is_none_or(|frame| {
                                frame.current.is_none()
                                    || frame.device.as_ref().is_none_or(|d| d.id == device)
                            })
                    })
            })
        else {
            self.trace_owned_interruption(provider, generation, sequence, device, false);
            return Ok(());
        };
        let selected = saved.selection.clone();
        self.trace_owned_interruption(provider, generation, sequence, device, true);
        self.cancel_deferred_spatial_play();
        self.clear_host_analysis();
        self.remote.owned_pause.clear();
        self.remote.resume_pending = None;
        self.remote.confirmed_resume_pause = None;
        self.remote.owned_recovery.cancel_provenance("promoted");
        self.remote.owned_recovery.watermark = (generation, u64::MAX);
        self.remote.owned_recovery.interrupted = Some(InterruptedSelection {
            provider: provider.into(),
            device: device.into(),
            generation,
        });
        self.remote.restored = Some(selected.clone());
        let mut stored = selected;
        stored.current.artwork = None;
        self.remote.last_selection = Some(stored);
        self.wanted_playing = false;
        self.session_state.restored = true;
        self.session_dirty = true;
        Ok(())
    }
}
