use super::*;
use provider_mock::{MockProvider, demo_tracks};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct SessionMailbox {
    updates: VecDeque<SessionUpdate>,
    saves: Vec<TransportSession>,
    selections: Vec<Option<RemoteSelection>>,
}
struct FixtureSessions(Arc<Mutex<SessionMailbox>>);
impl SessionStore for FixtureSessions {
    fn submit(&mut self, action: SessionAction) -> Result<(), PlayerError> {
        let mut mailbox = self.0.lock().unwrap();
        match action {
            SessionAction::Save { session, .. } => {
                mailbox.selections.push(session.remote.clone());
                mailbox.saves.push(*session);
            }
            SessionAction::SaveRemote { selection, .. } => {
                mailbox.selections.push(selection.map(|s| *s))
            }
            _ => {}
        }
        Ok(())
    }
    fn poll(&mut self) -> Option<SessionUpdate> {
        self.0.lock().unwrap().updates.pop_front()
    }
}
fn saved_remote() -> RemoteSelection {
    let mut current = state(1, false).playback.unwrap().current.unwrap();
    current.duration_ms = 180_000;
    current.location = Some(MediaLocation::Remote {
        uri: current.id.as_str().into(),
        url: "https://example.invalid/original-recording".into(),
        service: "Original fixture".into(),
    });
    RemoteSelection {
        provider: "fixture".into(),
        account_id: "original-account-a".into(),
        stable_account_id: None,
        current,
        position_ms: 42_000,
        context_uri: Some("fixture:playlist:original".into()),
    }
}
fn session_restore_setup() -> (
    PlaybackCoordinator,
    Arc<Mutex<FixtureMailbox>>,
    Arc<Mutex<SessionMailbox>>,
) {
    let mut core = PlaybackCoordinator::default();
    core.register(MockProvider::default());
    let network = Arc::new(Mutex::new(FixtureMailbox::default()));
    core.register_remote(FixtureRemote(network.clone()));
    let storage = Arc::new(Mutex::new(SessionMailbox::default()));
    core.register_session_store(FixtureSessions(storage.clone()));
    core.send(PlayerCommand::InitializeSession {
        directory: "/original-fixture-session".into(),
        access: SessionAccess::default(),
    })
    .unwrap();
    (core, network, storage)
}
fn load_selection(core: &mut PlaybackCoordinator, storage: &Arc<Mutex<SessionMailbox>>) {
    load_remote_selection(core, storage, saved_remote());
}
fn load_remote_selection(
    core: &mut PlaybackCoordinator,
    storage: &Arc<Mutex<SessionMailbox>>,
    selection: RemoteSelection,
) {
    let mut queue = QueueEngine::default();
    queue
        .replace(demo_tracks(), 1, PlaybackContext::UserQueue)
        .unwrap();
    storage
        .lock()
        .unwrap()
        .updates
        .push_back(SessionUpdate::Loaded {
            session: Some(Box::new(TransportSession {
                version: 2,
                remote: Some(selection),
                queue,
                position_ms: 12_345,
                volume: 0.4,
                stopped: false,
                root_paths: vec![],
            })),
            issues: vec![],
        });
    core.tick(Duration::ZERO);
}
fn bind_profile(
    core: &mut PlaybackCoordinator,
    network: &Arc<Mutex<FixtureMailbox>>,
    account: &str,
) {
    bind_profile_identity(core, network, account, None);
}
fn bind_profile_identity(
    core: &mut PlaybackCoordinator,
    network: &Arc<Mutex<FixtureMailbox>>,
    profile: &str,
    stable: Option<&str>,
) {
    network
        .lock()
        .unwrap()
        .updates
        .push_back(RemoteUpdate::Page(RemotePage {
            position_edits: false,
            related_collections: vec![],
            detail: true,
            item_positions: vec![],
            next_cursor: None,
            provider: "fixture".into(),
            query_id: 1,
            offset: 0,
            total: 1,
            items: vec![],
            spoken_progress: vec![],
            recent_plays: vec![],
            collections: vec![RemoteCollection {
                kind: RemoteEntityKind::Profile,
                account_id: stable.map(str::to_owned),
                owner_id: None,
                collaborative: None,
                public: None,
                subtitle: String::new(),
                description: String::new(),
                id: profile.into(),
                title: "Original fixture listener".into(),
                url: "https://example.invalid/".into(),
                artwork: None,
            }],
            issue: None,
        }));
    core.tick(Duration::ZERO);
}
fn output_state(observation: u64, device: &str, position: u64, playing: bool) -> RemoteState {
    let mut result = state(observation, playing);
    let frame = result.playback.as_mut().unwrap();
    frame.current = Some(saved_remote().current);
    frame.position_ms = position;
    frame.device = Some(PlaybackDevice {
        id: device.into(),
        name: "Original output".into(),
        device_type: "Computer".into(),
        volume: Some(0.3),
        is_active: true,
        is_restricted: false,
    });
    result
}
fn request_saved_resume(core: &mut PlaybackCoordinator, operation_id: Option<u64>) {
    let selected = saved_remote();
    core.send(PlayerCommand::Remote {
        provider: selected.provider,
        action: RemoteAction::Control {
            control: RemoteControl::ResumeOnDevice {
                operation_id,
                item: selected.current,
                device_id: "owned".into(),
                position_ms: selected.position_ms,
                context_uri: selected.context_uri,
            },
        },
    })
    .unwrap();
}
fn adopt_output(core: &mut PlaybackCoordinator) {
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::AdoptOutput,
    })
    .unwrap();
}

fn owned_pause_clock(
    core: &mut PlaybackCoordinator,
    sequence: u64,
    active: bool,
    position_ms: u64,
    media_id: MediaId,
) {
    owned_transport_clock(
        core,
        sequence,
        active,
        position_ms,
        media_id,
        PlaybackState::Paused,
        None,
    );
}

fn owned_transport_clock(
    core: &mut PlaybackCoordinator,
    sequence: u64,
    active: bool,
    position_ms: u64,
    media_id: MediaId,
    state: PlaybackState,
    control_uptime_ms: Option<u64>,
) {
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::OutputClock {
            clock: Some(RemoteOutputClock {
                generation: 1,
                sequence,
                device_id: "owned".into(),
                media_id: Some(media_id),
                state,
                position_ms,
                rate: if state == PlaybackState::Playing {
                    1.0
                } else {
                    0.0
                },
                active,
                volume: None,
                options: None,
                actions: None,
                metadata: None,
                control_uptime_ms,
                queue_epoch: None,
                confirmed_same_media_next: None,
            }),
        },
    })
    .unwrap();
}

fn owned_paused_state(observation: u64, position_ms: u64) -> RemoteState {
    let mut state = output_state(observation, "owned", position_ms, false);
    state.owned_output_observed = true;
    state
}

fn empty_remote_state(observation: u64) -> RemoteState {
    let mut state = output_state(observation, "owned", 0, false);
    state
        .devices
        .extend(state.playback.take().and_then(|frame| frame.device));
    state
}

fn live_owned_pause() -> (PlaybackCoordinator, Arc<Mutex<FixtureMailbox>>) {
    let (mut core, mailbox) = coordinator();
    bind_profile(&mut core, &mailbox, "original-account-a");
    owned_pause_clock(&mut core, 1, true, 42_000, saved_remote().current.id);
    update(&mut core, &mailbox, owned_paused_state(2, 42_000));
    mailbox.lock().unwrap().actions.clear();
    (core, mailbox)
}

fn crash_recovery_clock(sequence: u64, playing: bool, position_ms: u64) -> RemoteOutputClock {
    let selected = saved_remote();
    RemoteOutputClock {
        generation: 1,
        sequence,
        device_id: "owned".into(),
        media_id: Some(selected.current.id),
        state: if playing {
            PlaybackState::Playing
        } else {
            PlaybackState::Paused
        },
        position_ms,
        rate: if playing { 1.0 } else { 0.0 },
        active: true,
        volume: None,
        options: None,
        actions: Some(RemoteOutputActions::default()),
        metadata: Some(RemoteOutputMetadata {
            title: "Original crash fixture".into(),
            artists: vec!["Original artist".into()],
            album: None,
            duration_ms: selected.current.duration_ms,
            artwork: None,
            context_uri: selected.context_uri,
        }),
        control_uptime_ms: Some(10_000 + sequence * 250),
        queue_epoch: Some(7),
        confirmed_same_media_next: None,
    }
}

fn send_crash_clock(core: &mut PlaybackCoordinator, clock: RemoteOutputClock) {
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::OutputClock { clock: Some(clock) },
    })
    .unwrap();
}

fn live_crash_selection() -> (PlaybackCoordinator, Arc<Mutex<FixtureMailbox>>) {
    let (mut core, mailbox) = coordinator();
    bind_profile_identity(
        &mut core,
        &mailbox,
        "original-account-a",
        Some("stable-original-a"),
    );
    send_crash_clock(&mut core, crash_recovery_clock(1, true, 42_000));
    let mut actual = output_state(2, "owned", 42_000, true);
    actual.owned_output_observed = true;
    actual.owned_output_control = true;
    actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, actual);
    assert!(core.remote.restored.is_none());
    mailbox.lock().unwrap().actions.clear();
    (core, mailbox)
}

fn lose_crash_clock(core: &mut PlaybackCoordinator, mailbox: &Arc<Mutex<FixtureMailbox>>) {
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::OutputClock { clock: None },
    })
    .unwrap();
    update(core, mailbox, empty_remote_state(10));
}

fn interrupt_crash_child(core: &mut PlaybackCoordinator, sequence: u64) {
    core.send(PlayerCommand::OwnedOutputInterrupted {
        provider: "fixture".into(),
        generation: 1,
        sequence,
        device_id: "owned".into(),
    })
    .unwrap();
}

#[test]
fn owned_child_crash_restores_played_selection_only_after_explicit_exit_in_either_order() {
    for nil_first in [false, true] {
        let (mut core, mailbox) = live_crash_selection();
        if nil_first {
            lose_crash_clock(&mut core, &mailbox);
            assert!(
                core.snapshot().restored_remote.is_none(),
                "clock loss does not prove Pause"
            );
            assert_eq!(core.snapshot().state, PlaybackState::Idle);
        }
        interrupt_crash_child(&mut core, 1);
        if !nil_first {
            lose_crash_clock(&mut core, &mailbox);
        }
        let restored = core.snapshot();
        assert_eq!(restored.state, PlaybackState::Paused);
        assert_eq!(
            restored.queue.current.as_ref().unwrap().id,
            saved_remote().current.id
        );
        assert!((42_000..42_250).contains(&restored.position_ms));
        assert_eq!(
            restored.restored_remote.unwrap().context_uri,
            saved_remote().context_uri
        );
        assert!(restored.capabilities.play);
        assert!(!restored.capabilities.pause && !restored.capabilities.seek);
        assert!(!restored.playback_session_active);
        assert!(restored.host_analysis_status.is_none() && restored.host_spatial_status.is_none());
        assert!(restored.queue.upcoming.is_empty() && restored.queue.device_queue.is_none());
        assert!(
            mailbox
                .lock()
                .unwrap()
                .actions
                .iter()
                .all(|action| !matches!(action, RemoteAction::Control { .. }))
        );
        assert!(
            core.send(PlayerCommand::Play).is_err(),
            "saved output requires explicit device Resume"
        );
        assert!(core.send(PlayerCommand::Next).is_err());

        let mut new_runtime = crash_recovery_clock(1, false, 0);
        new_runtime.generation = 2;
        new_runtime.active = false;
        send_crash_clock(&mut core, new_runtime);
        update(&mut core, &mailbox, empty_remote_state(11));
        assert_eq!(core.snapshot().position_ms, restored.position_ms);
        assert!(core.snapshot().restored_remote.is_some());
        assert!(!core.snapshot().playback_session_active);
    }
}

#[test]
fn owned_child_crash_preserves_an_ordinary_confirmed_pause_without_prior_restore() {
    let (mut core, mailbox) = live_crash_selection();
    core.send(PlayerCommand::Pause).unwrap();
    send_crash_clock(&mut core, crash_recovery_clock(2, false, 42_500));
    let mut paused = owned_paused_state(3, 42_500);
    paused.playback.as_mut().unwrap().position_rate = 0.0;
    paused.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, paused);
    assert!(core.remote.restored.is_none());
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 2);
    assert_eq!(core.snapshot().state, PlaybackState::Paused);
    assert_eq!(core.snapshot().position_ms, 42_500);
    assert_eq!(
        core.snapshot().restored_remote.unwrap().context_uri,
        saved_remote().context_uri
    );
}

#[test]
fn owned_child_crash_corrects_a_projected_pause_anchor_without_reimporting_presentation_offset() {
    let (mut core, mailbox) = live_crash_selection();
    let actual = core.remote.states.get("fixture").unwrap().clone();
    core.remote.anchors.insert(
        "fixture".into(),
        (
            actual.observation_id,
            Instant::now() - Duration::from_secs(5),
        ),
    );
    assert!(core.snapshot().position_ms >= 46_000);
    core.observe_owned_recovery_state("fixture", &actual);
    core.send(PlayerCommand::Pause).unwrap();
    send_crash_clock(&mut core, crash_recovery_clock(2, false, 39_000));
    let mut paused = owned_paused_state(3, 39_200);
    paused.playback.as_mut().unwrap().position_rate = 0.0;
    paused.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, paused);
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 2);
    assert_eq!(
        core.snapshot().position_ms,
        39_000,
        "A corroborated Paused checkpoint retains the accepted native position"
    );
}

#[test]
fn owned_child_crash_accepts_a_corroborated_paused_anchor_in_the_existing_played_lineage() {
    let (mut core, mailbox) = live_crash_selection();
    core.send(PlayerCommand::Pause).unwrap();
    send_crash_clock(&mut core, crash_recovery_clock(2, false, 39_000));
    let mut paused = owned_paused_state(3, 39_000);
    paused.playback.as_mut().unwrap().position_rate = 0.0;
    paused.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, paused);
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 2);
    assert!(core.snapshot().restored_remote.is_some());
    assert_eq!(
        core.snapshot().position_ms,
        39_000,
        "Full matching owned Pause corrects an unproven projected/backward offset"
    );
    assert!(
        !core.snapshot().playback_session_active && core.snapshot().host_analysis_status.is_none()
    );
}

#[test]
fn owned_child_crash_corroborated_pause_rejects_stale_foreign_partial_and_seek_uncertainty() {
    for invalid in [
        "expired",
        "unowned",
        "wrong_position",
        "wrong_epoch",
        "wrong_context",
        "wrong_device",
        "partial",
        "seek",
        "selection",
    ] {
        let (mut core, mailbox) = live_crash_selection();
        match invalid {
            "expired" => core.remote.owned_recovery.expire_checkpoint(),
            "seek" => core.send(PlayerCommand::Seek { position_ms: 0 }).unwrap(),
            "selection" => core.send(PlayerCommand::Next).unwrap(),
            _ => {}
        }
        let mut clock = crash_recovery_clock(2, false, 39_000);
        let mut paused = owned_paused_state(3, 39_000);
        paused.playback.as_mut().unwrap().position_rate = 0.0;
        paused.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
        match invalid {
            "unowned" => paused.owned_output_observed = false,
            "wrong_position" => paused.playback.as_mut().unwrap().position_ms = 40_000,
            "wrong_epoch" => clock.queue_epoch = Some(8),
            "wrong_context" => {
                clock.metadata.as_mut().unwrap().context_uri = Some("fixture:album:other".into());
                paused.playback.as_mut().unwrap().context_uri = Some("fixture:album:other".into());
            }
            "wrong_device" => {
                clock.device_id = "foreign".into();
                paused
                    .playback
                    .as_mut()
                    .unwrap()
                    .device
                    .as_mut()
                    .unwrap()
                    .id = "foreign".into();
            }
            "partial" => {
                clock.actions = None;
                clock.metadata = None;
            }
            _ => {}
        }
        send_crash_clock(&mut core, clock);
        update(&mut core, &mailbox, paused);
        lose_crash_clock(&mut core, &mailbox);
        interrupt_crash_child(&mut core, 2);
        assert_ne!(
            core.snapshot().position_ms,
            39_000,
            "A paused anchor is not newly trusted for {invalid}"
        );
    }
}

#[test]
fn owned_child_crash_rejects_stale_scope_deadline_and_selection_intents() {
    for invalidation in [
        "expired",
        "restart",
        "next",
        "local",
        "transfer",
        "selection",
        "credential",
        "account",
        "foreign",
        "loading",
        "backward",
    ] {
        let (mut core, mailbox) = live_crash_selection();
        match invalidation {
            "expired" => core.remote.owned_recovery.expire_checkpoint(),
            "restart" => {
                core.send(PlayerCommand::CancelOwnedOutputRecovery {
                    provider: "fixture".into(),
                })
                .unwrap();
            }
            "next" => {
                core.send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::Control {
                        control: RemoteControl::Next,
                    },
                })
                .unwrap();
                send_crash_clock(&mut core, crash_recovery_clock(2, true, 42_050));
                let mut old = output_state(3, "owned", 42_050, true);
                old.owned_output_observed = true;
                old.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
                update(&mut core, &mailbox, old);
            }
            "credential" => {
                core.send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::Credential {
                        access_token: "original-test-bearer".into(),
                    },
                })
                .unwrap();
            }
            "local" => {
                let _ = core.send(local_selection());
            }
            "transfer" => {
                core.send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::Control {
                        control: RemoteControl::Transfer {
                            device_id: "phone".into(),
                            operation_id: None,
                        },
                    },
                })
                .unwrap();
            }
            "selection" => {
                core.send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::Control {
                        control: RemoteControl::Start {
                            item: saved_remote().current,
                        },
                    },
                })
                .unwrap();
            }
            "account" => bind_profile_identity(
                &mut core,
                &mailbox,
                "original-account-b",
                Some("stable-original-b"),
            ),
            "foreign" => update(&mut core, &mailbox, output_state(3, "phone", 42_050, true)),
            "loading" => {
                let mut next = crash_recovery_clock(2, true, 0);
                next.state = PlaybackState::Loading;
                next.rate = 0.0;
                send_crash_clock(&mut core, next);
            }
            "backward" => {
                let mut next = crash_recovery_clock(2, true, 42_050);
                next.control_uptime_ms = Some(1);
                send_crash_clock(&mut core, next);
            }
            _ => unreachable!(),
        }
        lose_crash_clock(&mut core, &mailbox);
        interrupt_crash_child(
            &mut core,
            if matches!(invalidation, "next" | "loading" | "backward") {
                2
            } else {
                1
            },
        );
        assert!(core.snapshot().restored_remote.is_none(), "{invalidation}");
    }
    let (mut core, mailbox) = live_crash_selection();
    lose_crash_clock(&mut core, &mailbox);
    for (generation, sequence, device) in [(2, 1, "owned"), (1, 2, "owned"), (1, 1, "phone")] {
        core.send(PlayerCommand::OwnedOutputInterrupted {
            provider: "fixture".into(),
            generation,
            sequence,
            device_id: device.into(),
        })
        .unwrap();
        assert!(core.snapshot().restored_remote.is_none());
    }
    interrupt_crash_child(&mut core, 1);
    assert!(core.snapshot().restored_remote.is_some());
}

#[test]
fn owned_child_crash_does_not_seed_from_unplayed_pause_or_incomplete_playing() {
    for kind in [
        "paused",
        "partial",
        "inactive",
        "zero_rate",
        "http_only",
        "no_account",
    ] {
        let (mut core, mailbox) = coordinator();
        if kind != "no_account" {
            bind_profile(&mut core, &mailbox, "original-account-a");
        }
        let mut clock = crash_recovery_clock(1, kind != "paused", 42_000);
        if kind == "partial" {
            clock.actions = None;
        }
        if kind == "inactive" {
            clock.active = false;
        }
        if kind == "zero_rate" {
            clock.rate = 0.0;
        }
        send_crash_clock(&mut core, clock);
        let mut actual = output_state(2, "owned", 42_000, kind != "paused");
        actual.owned_output_observed = kind != "http_only";
        actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
        update(&mut core, &mailbox, actual);
        lose_crash_clock(&mut core, &mailbox);
        interrupt_crash_child(&mut core, 1);
        assert!(core.snapshot().restored_remote.is_none(), "{kind}");
    }
}

#[test]
fn owned_child_crash_accepts_later_same_scope_partial_without_renewing_full_proof() {
    for expired in [false, true] {
        let (mut core, mailbox) = live_crash_selection();
        let mut partial = crash_recovery_clock(2, true, 42_050);
        partial.actions = None;
        send_crash_clock(&mut core, partial);
        if expired {
            core.remote.owned_recovery.expire_checkpoint();
        }
        lose_crash_clock(&mut core, &mailbox);
        interrupt_crash_child(&mut core, 2);
        assert_eq!(core.snapshot().restored_remote.is_some(), !expired);
    }
}

#[test]
fn owned_child_crash_restored_selection_yields_to_takeover_and_requires_exact_explicit_resume() {
    let (mut core, mailbox) = live_crash_selection();
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 1);
    let selected = core.remote.restored.clone().unwrap();
    let resume = |device: &str| PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::ResumeOnDevice {
                item: selected.current.clone(),
                device_id: device.into(),
                position_ms: selected.position_ms,
                context_uri: selected.context_uri.clone(),
                operation_id: Some(1),
            },
        },
    };
    assert!(core.send(resume("phone")).is_err());
    core.send(resume("owned")).unwrap();
    assert!(core.snapshot().restored_remote.is_some());
    assert!(mailbox.lock().unwrap().actions.iter().any(|action| matches!(action, RemoteAction::Control { control: RemoteControl::ResumeOnDevice { device_id, position_ms, .. } } if device_id == "owned" && *position_ms == selected.position_ms)));
    update(&mut core, &mailbox, output_state(12, "phone", 1000, true));
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().state, PlaybackState::Playing);
    assert!(core.snapshot().host_analysis_status.is_none());
}

#[test]
fn owned_child_crash_yields_to_an_inactive_foreign_selection_before_or_after_exit() {
    for exited in [false, true] {
        let (mut core, mailbox) = live_crash_selection();
        if exited {
            interrupt_crash_child(&mut core, 1);
        }
        let mut foreign = output_state(4, "phone", 12_000, false);
        foreign
            .playback
            .as_mut()
            .unwrap()
            .device
            .as_mut()
            .unwrap()
            .is_active = false;
        update(&mut core, &mailbox, foreign);
        interrupt_crash_child(&mut core, 1);
        assert!(core.snapshot().restored_remote.is_none());
        assert_eq!(core.snapshot().position_ms, 12_000);
    }
}

#[test]
fn owned_child_crash_can_seed_again_from_fresh_actual_playing_after_a_passive_gap() {
    let (mut core, mailbox) = live_crash_selection();
    lose_crash_clock(&mut core, &mailbox);
    core.remote.owned_recovery.expire_checkpoint();
    let mut fresh = crash_recovery_clock(2, true, 54_000);
    fresh.control_uptime_ms = Some(22_000);
    send_crash_clock(&mut core, fresh);
    let mut actual = output_state(11, "owned", 54_000, true);
    actual.owned_output_observed = true;
    actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, actual);
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 1);
    assert!(
        core.snapshot().restored_remote.is_none(),
        "old exit cannot restore the new proof"
    );
    interrupt_crash_child(&mut core, 2);
    assert!((54_000..54_250).contains(&core.snapshot().position_ms));
    assert!(core.snapshot().restored_remote.is_some());
}

#[test]
fn owned_child_crash_pause_seek_uses_only_a_confirmed_position() {
    for confirmed in [false, true] {
        let (mut core, mailbox) = live_crash_selection();
        send_crash_clock(&mut core, crash_recovery_clock(2, false, 42_500));
        let mut paused = owned_paused_state(3, 42_500);
        paused.playback.as_mut().unwrap().position_rate = 0.0;
        paused.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
        update(&mut core, &mailbox, paused);
        core.send(PlayerCommand::Seek {
            position_ms: 90_000,
        })
        .unwrap();
        if confirmed {
            send_crash_clock(&mut core, crash_recovery_clock(3, false, 90_000));
            let mut actual = owned_paused_state(4, 90_000);
            actual.playback.as_mut().unwrap().position_rate = 0.0;
            actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
            update(&mut core, &mailbox, actual);
        } else {
            // A provider may optimistically project its requested seek. Without
            // a matching actual output clock this must not become a checkpoint.
            let mut optimistic = owned_paused_state(4, 90_000);
            optimistic.playback.as_mut().unwrap().position_rate = 0.0;
            optimistic.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
            update(&mut core, &mailbox, optimistic);
        }
        lose_crash_clock(&mut core, &mailbox);
        interrupt_crash_child(&mut core, if confirmed { 3 } else { 2 });
        assert_eq!(
            core.snapshot().position_ms,
            if confirmed { 90_000 } else { 42_500 }
        );
    }
}

#[test]
fn owned_child_crash_preserves_saved_selection_through_late_results_and_explicit_engine_retry() {
    let (mut core, mailbox) = live_crash_selection();
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 1);
    let saved = core.remote.restored.clone().unwrap();
    core.send(PlayerCommand::CancelOwnedOutputRecovery {
        provider: "fixture".into(),
    })
    .unwrap();
    core.send(PlayerCommand::HostAnalysis { lease: None })
        .unwrap();
    mailbox
        .lock()
        .unwrap()
        .updates
        .push_back(RemoteUpdate::OutputCommandFailed {
            control: RemoteOutputControl::Pause,
            error: PlayerError::unavailable("fixture"),
        });
    core.tick(Duration::ZERO);
    assert_eq!(core.snapshot().position_ms, saved.position_ms);
    assert!(core.snapshot().restored_remote.is_some());
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::ResumeOnDevice {
                item: saved.current.clone(),
                position_ms: saved.position_ms,
                context_uri: saved.context_uri.clone(),
                operation_id: Some(1),
                device_id: "owned".into(),
            },
        },
    })
    .unwrap();
    let mut failure = empty_remote_state(11);
    failure.control_result = Some(RemoteControlResult {
        operation_id: 1,
        error: Some(PlayerError::unavailable("fixture")),
    });
    update(&mut core, &mailbox, failure);
    assert!(core.snapshot().restored_remote.is_some());
    assert_eq!(core.snapshot().position_ms, saved.position_ms);
    assert!(!core.snapshot().playback_session_active);
}

#[test]
fn owned_child_crash_explicit_resume_confirms_and_can_recover_the_new_actual_runtime() {
    let (mut core, mailbox) = live_crash_selection();
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 1);
    let selected = core.remote.restored.clone().unwrap();
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::ResumeOnDevice {
                item: selected.current,
                position_ms: selected.position_ms,
                context_uri: selected.context_uri,
                operation_id: Some(1),
                device_id: "owned".into(),
            },
        },
    })
    .unwrap();
    let mut clock = crash_recovery_clock(1, true, 42_400);
    clock.generation = 2;
    send_crash_clock(&mut core, clock);
    let mut actual = output_state(11, "owned", 42_400, true);
    actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    actual.owned_output_observed = true;
    actual.control_result = Some(RemoteControlResult {
        operation_id: 1,
        error: None,
    });
    update(&mut core, &mailbox, actual);
    assert!(core.snapshot().restored_remote.is_none());
    core.send(PlayerCommand::OwnedOutputInterrupted {
        provider: "fixture".into(),
        generation: 2,
        sequence: 1,
        device_id: "owned".into(),
    })
    .unwrap();
    assert!(core.snapshot().restored_remote.is_some());
    assert!((42_400..42_650).contains(&core.snapshot().position_ms));
    assert!(!core.snapshot().playback_session_active);
}

#[test]
fn owned_child_crash_account_reverification_requires_a_new_actual_played_proof() {
    let (mut core, mailbox) = live_crash_selection();
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Credential {
            access_token: "original-refreshed-test-bearer".into(),
        },
    })
    .unwrap();
    bind_profile_identity(
        &mut core,
        &mailbox,
        "original-account-a",
        Some("stable-original-a"),
    );
    interrupt_crash_child(&mut core, 1);
    assert!(core.snapshot().restored_remote.is_none());
    send_crash_clock(&mut core, crash_recovery_clock(2, true, 42_250));
    let mut actual = output_state(3, "owned", 42_250, true);
    actual.owned_output_observed = true;
    actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, actual);
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 1);
    assert!(core.snapshot().restored_remote.is_none());
    interrupt_crash_child(&mut core, 2);
    assert!(core.snapshot().restored_remote.is_some());
}

#[test]
fn owned_child_crash_account_switch_cannot_revalidate_the_old_paused_lineage() {
    let (mut core, mailbox) = live_crash_selection();
    bind_profile_identity(
        &mut core,
        &mailbox,
        "original-account-b",
        Some("stable-original-b"),
    );
    send_crash_clock(&mut core, crash_recovery_clock(2, false, 42_500));
    bind_profile_identity(
        &mut core,
        &mailbox,
        "original-account-a",
        Some("stable-original-a"),
    );
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 2);
    assert!(core.snapshot().restored_remote.is_none());
}

#[test]
fn owned_child_crash_accepts_the_last_host_acked_sequence_of_the_same_played_lineage() {
    let (mut core, mailbox) = live_crash_selection();
    send_crash_clock(&mut core, crash_recovery_clock(2, true, 42_250));
    let mut actual = output_state(3, "owned", 42_250, true);
    actual.owned_output_observed = true;
    actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
    update(&mut core, &mailbox, actual);
    // Core accepted seq2, but its acknowledgement has not reached MainActor.
    // The host can confirm only seq1 when the exact same actual child exits.
    lose_crash_clock(&mut core, &mailbox);
    interrupt_crash_child(&mut core, 1);
    assert!(core.snapshot().restored_remote.is_some());
    assert!((42_250..42_500).contains(&core.snapshot().position_ms));
}

#[test]
fn owned_child_crash_yields_only_to_a_full_active_new_runtime_paused_selection() {
    for kind in [
        "different_item",
        "different_context",
        "same_zero",
        "inactive",
        "partial",
        "cloud_only",
    ] {
        let (mut core, mailbox) = live_crash_selection();
        lose_crash_clock(&mut core, &mailbox);
        interrupt_crash_child(&mut core, 1);
        let saved_position = core.snapshot().position_ms;
        let mut clock = crash_recovery_clock(1, false, 0);
        clock.generation = 2;
        let mut actual = owned_paused_state(11, 0);
        actual.playback.as_mut().unwrap().position_rate = 0.0;
        actual.playback.as_mut().unwrap().context_uri = saved_remote().context_uri;
        if kind != "same_zero" {
            let replacement = MediaId::parse("fixture:track:replacement").unwrap();
            clock.media_id = Some(replacement.clone());
            actual
                .playback
                .as_mut()
                .unwrap()
                .current
                .as_mut()
                .unwrap()
                .id = replacement;
        }
        if kind == "different_context" {
            clock.media_id = Some(saved_remote().current.id.clone());
            actual
                .playback
                .as_mut()
                .unwrap()
                .current
                .as_mut()
                .unwrap()
                .id = saved_remote().current.id;
            let context = Some("fixture:album:replacement".into());
            clock
                .metadata
                .as_mut()
                .unwrap()
                .context_uri
                .clone_from(&context);
            actual.playback.as_mut().unwrap().context_uri = context;
        }
        if kind == "inactive" {
            clock.active = false;
            actual
                .playback
                .as_mut()
                .unwrap()
                .device
                .as_mut()
                .unwrap()
                .is_active = false;
        }
        if kind == "partial" {
            clock.actions = None;
        }
        if kind == "cloud_only" {
            actual.owned_output_observed = false;
        }
        send_crash_clock(&mut core, clock);
        update(&mut core, &mailbox, actual);
        let snapshot = core.snapshot();
        let yields = matches!(kind, "different_item" | "different_context");
        assert_eq!(snapshot.restored_remote.is_none(), yields, "{kind}");
        assert_eq!(
            snapshot.position_ms,
            if yields { 0 } else { saved_position },
            "{kind}"
        );
        assert!(!snapshot.capabilities.system_media_session && !snapshot.capabilities.pcm_analysis);
        assert!(snapshot.host_analysis_status.is_none() && snapshot.host_spatial_status.is_none());
    }
}

fn dormant_resume(device: &str, position_ms: u64, operation_id: u64) -> PlayerCommand {
    let item = saved_remote().current;
    PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::ResumeOnDevice {
                item,
                device_id: device.into(),
                position_ms,
                context_uri: None,
                operation_id: Some(operation_id),
            },
        },
    }
}

#[test]
fn dormant_album_pause_survives_device_discovery_gap_without_admitting_resume() {
    let media = saved_remote().current.id;
    let context = "fixture:album:original";
    for inactive_clock_first in [true, false] {
        let (mut core, mailbox) = coordinator();
        bind_profile(&mut core, &mailbox, "original-account-a");
        owned_pause_clock(&mut core, 1, true, 46_079, media.clone());
        let mut confirmed = owned_paused_state(2, 46_079);
        confirmed.playback.as_mut().unwrap().context_uri = Some(context.into());
        update(&mut core, &mailbox, confirmed);

        if inactive_clock_first {
            owned_pause_clock(&mut core, 2, false, 46_079, media.clone());
        }
        let mut missing = empty_remote_state(3);
        missing.devices.clear();
        update(&mut core, &mailbox, missing.clone());
        if !inactive_clock_first {
            assert!(core.snapshot().restored_remote.is_none());
            owned_pause_clock(&mut core, 2, false, 46_079, media.clone());
            missing.observation_id += 1;
            update(&mut core, &mailbox, missing);
        }
        let dormant = core.snapshot();
        assert_eq!(dormant.queue.current.unwrap().id, media);
        assert_eq!(dormant.position_ms, 46_079);
        assert_eq!(
            dormant.restored_remote.unwrap().context_uri.as_deref(),
            Some(context)
        );
        assert!(!dormant.capabilities.play && !dormant.playback_session_active);

        let resume = PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Control {
                control: RemoteControl::ResumeOnDevice {
                    item: saved_remote().current,
                    device_id: "owned".into(),
                    position_ms: 46_079,
                    context_uri: Some(context.into()),
                    operation_id: Some(29),
                },
            },
        };
        assert!(core.send(resume.clone()).is_err());
        let mut discovered = empty_remote_state(5);
        discovered.devices[0].is_active = false;
        update(&mut core, &mailbox, discovered);
        assert!(core.snapshot().capabilities.play);
        core.send(resume).unwrap();
    }
}

#[test]
fn dormant_album_pause_survives_same_inactive_device_without_item_or_context() {
    let (mut core, mailbox) = coordinator();
    let media = saved_remote().current.id;
    let context = "fixture:album:original";
    bind_profile(&mut core, &mailbox, "original-account-a");
    owned_pause_clock(&mut core, 1, true, 46_079, media.clone());
    let mut confirmed = owned_paused_state(2, 46_079);
    confirmed.playback.as_mut().unwrap().context_uri = Some(context.into());
    update(&mut core, &mailbox, confirmed);
    owned_pause_clock(&mut core, 2, false, 46_079, media.clone());

    let mut inert = output_state(3, "owned", 0, false);
    let frame = inert.playback.as_mut().unwrap();
    frame.current = None;
    frame.device.as_mut().unwrap().is_active = false;
    inert.devices.extend(frame.device.clone());
    update(&mut core, &mailbox, inert);
    let dormant = core.snapshot();
    assert_eq!(dormant.queue.current.unwrap().id, media);
    assert_eq!(dormant.position_ms, 46_079);
    assert_eq!(
        dormant.restored_remote.unwrap().context_uri.as_deref(),
        Some(context)
    );
    assert!(dormant.capabilities.play && !dormant.playback_session_active);

    update(&mut core, &mailbox, output_state(4, "phone", 55_000, true));
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().position_ms, 55_000);
    update(&mut core, &mailbox, empty_remote_state(5));
    assert!(core.snapshot().queue.current.is_none());
}

#[test]
fn dormant_owned_pause_keeps_selection_during_web_unavailability() {
    let (mut core, mailbox) = live_owned_pause();
    let media = saved_remote().current.id;
    owned_pause_clock(&mut core, 2, false, 42_000, media.clone());
    let mut unavailable = empty_remote_state(3);
    unavailable.connection = ProviderConnectionState::Unavailable;
    unavailable.devices[0].is_active = false;
    update(&mut core, &mailbox, unavailable);

    let snapshot = core.snapshot();
    assert_eq!(snapshot.queue.current.unwrap().id, media);
    assert_eq!(snapshot.position_ms, 42_000);
    assert_eq!(snapshot.state, PlaybackState::Paused);
    assert!(snapshot.restored_remote.is_some());
    assert!(!snapshot.capabilities.play && !snapshot.playback_session_active);
    assert!(core.send(dormant_resume("owned", 42_000, 1)).is_err());

    let mut connected = empty_remote_state(4);
    connected.devices[0].is_active = false;
    update(&mut core, &mailbox, connected);
    assert!(core.snapshot().capabilities.play);
    core.send(dormant_resume("owned", 42_000, 1)).unwrap();
}

#[test]
fn an_admitted_owned_pause_recovers_when_its_first_confirmation_is_inactive() {
    let (mut core, mailbox) = coordinator();
    let media = saved_remote().current.id;
    bind_profile(&mut core, &mailbox, "original-account-a");
    owned_transport_clock(
        &mut core,
        1,
        true,
        1_950,
        media.clone(),
        PlaybackState::Playing,
        Some(1_000),
    );
    let mut playing = output_state(2, "owned", 1_950, true);
    playing.owned_output_observed = true;
    playing.owned_output_control = true;
    update(&mut core, &mailbox, playing);

    core.send(PlayerCommand::Pause).unwrap();
    owned_pause_clock(&mut core, 2, false, 2_006, media.clone());
    update(&mut core, &mailbox, output_state(3, "owned", 1_950, true));
    assert!(core.snapshot().restored_remote.is_none());
    assert!(core.send(dormant_resume("owned", 2_006, 1)).is_err());

    let mut empty = empty_remote_state(4);
    empty.devices[0].is_active = false;
    update(&mut core, &mailbox, empty);
    let paused = core.snapshot();
    assert_eq!(paused.queue.current.unwrap().id, media);
    assert_eq!(paused.position_ms, 2_006);
    assert!(paused.restored_remote.is_some() && paused.capabilities.play);
    assert!(!paused.playback_session_active && !paused.capabilities.pcm_analysis);
    assert_eq!(
        core.remote_selection_for_storage().unwrap().position_ms,
        2_006
    );
    core.send(dormant_resume("owned", 2_006, 1)).unwrap();
}

#[test]
fn inactive_first_pause_needs_an_admitted_owned_pause_and_never_revives_a_retired_intent() {
    for interruption in [
        "no pause",
        "external",
        "next",
        "clock lost",
        "no control clock",
        "account changed",
    ] {
        let (mut core, mailbox) = coordinator();
        let media = saved_remote().current.id;
        bind_profile(&mut core, &mailbox, "original-account-a");
        owned_transport_clock(
            &mut core,
            1,
            true,
            1_950,
            media.clone(),
            PlaybackState::Playing,
            (interruption != "no control clock").then_some(1_000),
        );
        let mut playing = output_state(2, "owned", 1_950, true);
        playing.owned_output_observed = true;
        playing.owned_output_control = true;
        update(&mut core, &mailbox, playing);
        if interruption != "no pause" {
            core.send(PlayerCommand::Pause).unwrap();
        }
        match interruption {
            "external" => update(&mut core, &mailbox, output_state(3, "phone", 5_000, true)),
            "next" => core.send(PlayerCommand::Next).unwrap(),
            "clock lost" => core
                .send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::OutputClock { clock: None },
                })
                .unwrap(),
            "account changed" => {
                bind_profile(&mut core, &mailbox, "original-account-b");
                bind_profile(&mut core, &mailbox, "original-account-a");
            }
            _ => (),
        }
        owned_pause_clock(&mut core, 2, false, 2_006, media);
        update(&mut core, &mailbox, empty_remote_state(4));
        assert!(
            core.snapshot().restored_remote.is_none(),
            "revived after {interruption}"
        );
        assert!(core.send(dormant_resume("owned", 2_006, 1)).is_err());
    }
}

#[test]
fn long_paused_owned_output_restores_exact_seek_after_deactivation() {
    let (mut core, mailbox) = live_owned_pause();
    let item = saved_remote().current;
    core.send(PlayerCommand::Seek {
        position_ms: 46_598,
    })
    .unwrap();
    owned_pause_clock(&mut core, 2, true, 42_000, item.id.clone());
    update(&mut core, &mailbox, owned_paused_state(3, 42_000));
    update(&mut core, &mailbox, empty_remote_state(4));
    assert!(
        core.snapshot().restored_remote.is_none(),
        "A stale paused position must not rearm a seek"
    );
    owned_pause_clock(&mut core, 3, true, 46_597, item.id.clone());
    update(&mut core, &mailbox, owned_paused_state(5, 46_597));
    owned_pause_clock(&mut core, 4, false, 46_597, item.id.clone());
    update(&mut core, &mailbox, empty_remote_state(6));
    core.tick(Duration::from_secs(16 * 60));
    let paused = core.snapshot();
    assert_eq!(paused.state, PlaybackState::Paused);
    assert_eq!(paused.queue.current.unwrap().id, item.id);
    assert_eq!(paused.position_ms, 46_597);
    assert!(paused.restored_remote.is_some() && paused.capabilities.play);
    assert!(
        !paused.playback_session_active
            && !paused.capabilities.pause
            && !paused.capabilities.pcm_analysis
    );
    assert_eq!(
        core.remote_selection_for_storage().unwrap().position_ms,
        46_597
    );
    assert!(
        core.remote.restored.is_none(),
        "A live dormant selection must remain a projection until explicit Play"
    );
    assert!(core.send(PlayerCommand::Play).is_err());
    assert!(core.send(dormant_resume("phone", 46_597, 1)).is_err());
    assert!(core.send(dormant_resume("owned", 42_000, 1)).is_err());
    assert!(
        !mailbox
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|action| matches!(
                action,
                RemoteAction::Control {
                    control: RemoteControl::Play | RemoteControl::ResumeOnDevice { .. }
                }
            ))
    );
    core.send(dormant_resume("owned", 46_597, 1)).unwrap();
    assert!(
        matches!(mailbox.lock().unwrap().actions.last(), Some(RemoteAction::Control {
        control: RemoteControl::ResumeOnDevice { device_id, position_ms: 46_597, operation_id: Some(1), .. },
    }) if device_id == "owned")
    );
    let mut playing = output_state(7, "owned", 46_800, true);
    playing.control_result = Some(RemoteControlResult {
        operation_id: 1,
        error: None,
    });
    update(&mut core, &mailbox, playing);
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().state, PlaybackState::Playing);
}

#[test]
fn a_lagged_owned_http_pause_cannot_undo_a_confirmed_seek_after_deactivation() {
    let (mut core, mailbox) = live_owned_pause();
    let media = saved_remote().current.id;
    core.send(PlayerCommand::Seek {
        position_ms: 46_598,
    })
    .unwrap();
    owned_pause_clock(&mut core, 2, true, 46_597, media.clone());
    update(&mut core, &mailbox, owned_paused_state(3, 46_597));
    owned_pause_clock(&mut core, 3, false, 46_597, media.clone());

    let mut lagged = output_state(4, "owned", 28_631, false);
    lagged
        .devices
        .push(lagged.playback.as_ref().unwrap().device.clone().unwrap());
    update(&mut core, &mailbox, lagged);
    let snapshot = core.snapshot();
    assert_eq!(snapshot.queue.current.unwrap().id, media);
    assert_eq!(snapshot.position_ms, 46_597);
    assert!(snapshot.restored_remote.is_some() && snapshot.capabilities.play);
    assert!(!snapshot.playback_session_active && !snapshot.capabilities.seek);
    assert_eq!(
        core.remote_selection_for_storage().unwrap().position_ms,
        46_597
    );
    assert!(core.send(dormant_resume("owned", 28_631, 1)).is_err());

    let mut external = output_state(5, "phone", 73_000, false);
    external.devices.push(PlaybackDevice {
        id: "owned".into(),
        name: "Original output".into(),
        device_type: "Computer".into(),
        volume: Some(0.3),
        is_active: false,
        is_restricted: false,
    });
    update(&mut core, &mailbox, external);
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().position_ms, 73_000);
}

#[test]
fn a_new_context_retires_the_dormant_pause_even_when_the_track_and_device_match() {
    let (mut core, mailbox) = live_owned_pause();
    let media = saved_remote().current.id;
    owned_pause_clock(&mut core, 2, false, 42_000, media);
    update(&mut core, &mailbox, empty_remote_state(3));
    assert!(core.snapshot().restored_remote.is_some());

    let mut changed = output_state(4, "owned", 73_000, false);
    let frame = changed.playback.as_mut().unwrap();
    frame.context_uri = Some("fixture:playlist:second".into());
    changed.devices.extend(frame.device.clone());
    update(&mut core, &mailbox, changed);
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().position_ms, 73_000);

    update(&mut core, &mailbox, empty_remote_state(5));
    assert!(core.snapshot().restored_remote.is_none());
    assert!(core.send(dormant_resume("owned", 42_000, 1)).is_err());
    let checkpoint = core.remote_selection_for_storage().unwrap();
    assert_eq!(
        checkpoint.context_uri.as_deref(),
        Some("fixture:playlist:second")
    );
    assert_eq!(checkpoint.position_ms, 73_000);

    owned_pause_clock(&mut core, 3, true, 73_000, saved_remote().current.id);
    let mut verified = owned_paused_state(6, 73_000);
    verified.playback.as_mut().unwrap().context_uri = checkpoint.context_uri.clone();
    update(&mut core, &mailbox, verified);
    owned_pause_clock(&mut core, 4, false, 73_000, saved_remote().current.id);
    update(&mut core, &mailbox, empty_remote_state(7));
    let dormant = core.snapshot();
    assert_eq!(dormant.position_ms, 73_000);
    assert_eq!(
        dormant.restored_remote.unwrap().context_uri,
        checkpoint.context_uri
    );
}

#[test]
fn a_second_pause_recovers_when_its_first_confirmed_seek_clock_is_inactive() {
    for lose_clock in [false, true] {
        let (mut core, mailbox) = live_owned_pause();
        let media = saved_remote().current.id;
        owned_pause_clock(&mut core, 2, false, 42_000, media.clone());
        update(&mut core, &mailbox, empty_remote_state(3));
        core.send(dormant_resume("owned", 42_000, 1)).unwrap();
        let mut playing = output_state(4, "owned", 42_350, true);
        playing.owned_output_observed = true;
        playing.control_result = Some(RemoteControlResult {
            operation_id: 1,
            error: None,
        });
        update(&mut core, &mailbox, playing);
        assert!(core.snapshot().restored_remote.is_none());

        core.send(PlayerCommand::Pause).unwrap();
        owned_pause_clock(&mut core, 3, true, 42_400, media.clone());
        update(&mut core, &mailbox, owned_paused_state(5, 42_400));
        core.send(PlayerCommand::Seek {
            position_ms: 46_598,
        })
        .unwrap();
        owned_pause_clock(&mut core, 4, false, 42_400, media.clone());
        update(&mut core, &mailbox, empty_remote_state(6));
        assert!(
            core.snapshot().restored_remote.is_none(),
            "A stale seek position cannot become a resumable selection"
        );

        if lose_clock {
            core.send(PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::OutputClock { clock: None },
            })
            .unwrap();
        }
        owned_pause_clock(&mut core, 5, false, 46_597, media.clone());
        core.session_dirty = false;
        update(&mut core, &mailbox, empty_remote_state(7));
        let snapshot = core.snapshot();
        if lose_clock {
            assert!(
                snapshot.restored_remote.is_none(),
                "A cleared output cannot be revived by an inactive clock"
            );
            continue;
        }
        assert_eq!(snapshot.queue.current.unwrap().id, media);
        assert_eq!(snapshot.position_ms, 46_597);
        assert!(snapshot.restored_remote.is_some() && snapshot.capabilities.play);
        assert!(!snapshot.playback_session_active && !snapshot.capabilities.pcm_analysis);
        assert!(
            core.session_dirty,
            "The verified new paused position must be checkpointed"
        );
        assert_eq!(
            core.remote_selection_for_storage().unwrap().position_ms,
            46_597
        );
        core.session_dirty = false;
        update(&mut core, &mailbox, empty_remote_state(8));
        assert!(
            !core.session_dirty,
            "An empty poll must not rewrite the checkpoint"
        );
        core.send(dormant_resume("owned", 46_597, 2)).unwrap();
    }
}

#[test]
fn dormant_owned_pause_does_not_survive_mismatch_authentication_or_admitted_transfer() {
    for change in [
        "raw media",
        "raw clock loss",
        "external frame",
        "external discovery",
        "auth",
        "transfer",
        "missing device",
        "restricted device",
    ] {
        let (mut core, mailbox) = live_owned_pause();
        match change {
            "raw media" => {
                owned_pause_clock(
                    &mut core,
                    2,
                    true,
                    42_000,
                    MediaId::parse("fixture:track:another").unwrap(),
                );
                update(&mut core, &mailbox, owned_paused_state(3, 42_000));
            }
            "raw clock loss" => {
                core.send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::OutputClock { clock: None },
                })
                .unwrap();
                owned_pause_clock(&mut core, 2, false, 42_000, saved_remote().current.id);
            }
            "external frame" => {
                update(&mut core, &mailbox, output_state(3, "phone", 12_000, true));
                assert_eq!(core.snapshot().state, PlaybackState::Playing);
            }
            "external discovery" => {
                owned_pause_clock(&mut core, 2, false, 42_000, saved_remote().current.id);
                let mut state = empty_remote_state(3);
                state.devices[0].is_active = false;
                let external = output_state(3, "phone", 12_000, true);
                state.devices.extend(external.playback.unwrap().device);
                update(&mut core, &mailbox, state);
            }
            "auth" => {
                let mut state = empty_remote_state(3);
                state.authentication_required = true;
                update(&mut core, &mailbox, state);
                bind_profile(&mut core, &mailbox, "original-account-a");
            }
            "transfer" => {
                core.send(PlayerCommand::Remote {
                    provider: "fixture".into(),
                    action: RemoteAction::Control {
                        control: RemoteControl::Transfer {
                            device_id: "phone".into(),
                            operation_id: None,
                        },
                    },
                })
                .unwrap();
                owned_pause_clock(&mut core, 2, true, 42_000, saved_remote().current.id);
                update(&mut core, &mailbox, owned_paused_state(3, 42_000));
            }
            "missing device" | "restricted device" => {
                let mut state = empty_remote_state(3);
                if change == "missing device" {
                    state.devices.clear();
                } else {
                    state.devices[0].is_restricted = true;
                }
                update(&mut core, &mailbox, state);
                assert!(core.send(dormant_resume("owned", 42_000, 3)).is_err());
            }
            _ => unreachable!(),
        }
        update(&mut core, &mailbox, empty_remote_state(4));
        assert!(
            core.snapshot().queue.current.is_none(),
            "Dormant selection survived {change}"
        );
        assert!(core.send(dormant_resume("owned", 42_000, 3)).is_err());
    }
}

#[test]
fn a_failed_dormant_resume_can_retry_and_a_late_result_does_not_mask_external_playback() {
    let (mut core, mailbox) = live_owned_pause();
    owned_pause_clock(&mut core, 2, false, 42_000, saved_remote().current.id);
    update(&mut core, &mailbox, empty_remote_state(3));
    core.send(dormant_resume("owned", 42_000, 1)).unwrap();
    let mut failed = empty_remote_state(4);
    failed.control_result = Some(RemoteControlResult {
        operation_id: 1,
        error: Some(PlayerError::invalid("Original fixture failure")),
    });
    update(&mut core, &mailbox, failed);
    assert!(core.snapshot().restored_remote.is_some() && core.remote.restored.is_none());
    core.send(dormant_resume("owned", 42_000, 2)).unwrap();
    let mut external = output_state(5, "phone", 12_000, true);
    external.control_result = Some(RemoteControlResult {
        operation_id: 1,
        error: None,
    });
    update(&mut core, &mailbox, external.clone());
    assert!(core.snapshot().restored_remote.is_none());
    assert!((12_000..12_250).contains(&core.snapshot().position_ms));
    external.control_result = Some(RemoteControlResult {
        operation_id: 2,
        error: None,
    });
    update(&mut core, &mailbox, external);
    assert!(core.snapshot().restored_remote.is_none());
    assert!(core.remote.resume_pending.is_none());
}

#[test]
fn a_positioned_catalog_start_does_not_need_or_create_a_restored_selection() {
    for restoring in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        if restoring {
            load_selection(&mut core, &storage);
            bind_profile(&mut core, &network, "original-account-a");
            assert!(core.snapshot().restored_remote.is_some());
        }
        let previous = output_state(1, "other-output", 1000, false);
        let previous_id = previous
            .playback
            .as_ref()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .id
            .clone();
        update(&mut core, &network, previous);
        let mut chapter = saved_remote().current;
        chapter.id = MediaId::parse("fixture:chapter:original").unwrap();
        if let Some(MediaLocation::Remote { uri, .. }) = &mut chapter.location {
            *uri = chapter.id.as_str().into();
        }
        let command = |item| PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Control {
                control: RemoteControl::StartSpokenOnDevice {
                    item,
                    device_id: "owned".into(),
                    position_ms: 42_000,
                    operation_id: 31,
                },
            },
        };
        let mut foreign = chapter.clone();
        foreign.id = MediaId::parse("another:chapter:original").unwrap();
        assert!(core.send(command(foreign)).is_err());
        assert!(network.lock().unwrap().actions.is_empty());
        core.send(command(chapter.clone())).unwrap();
        assert!(
            matches!(network.lock().unwrap().actions.as_slice(), [RemoteAction::Control {
            control: RemoteControl::StartSpokenOnDevice { item, position_ms: 42_000, operation_id: 31, .. }
        }] if item.id == chapter.id)
        );
        let snapshot = core.snapshot();
        assert_eq!(snapshot.active_provider.as_deref(), Some("fixture"));
        assert!(snapshot.restored_remote.is_none());
        assert_eq!(snapshot.queue.current.unwrap().id, previous_id);
        assert!(core.remote.resume_pending.is_none());
    }
}

#[test]
fn remote_restore_waits_for_matching_identity_and_never_restores_a_queue_or_playback_lease() {
    for profile_first in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        update(&mut core, &network, output_state(1, "external", 1000, true));
        if profile_first {
            bind_profile(&mut core, &network, "original-account-a");
        }
        load_selection(&mut core, &storage);
        if !profile_first {
            assert!(core.snapshot().restored_remote.is_none());
            bind_profile(&mut core, &network, "original-account-a");
        }
        let restored = core.snapshot();
        assert!(restored.restored_remote.is_some());
        assert_eq!(restored.state, PlaybackState::Paused);
        assert_eq!(restored.position_ms, 42_000);
        assert!(
            !restored.playback_session_active
                && !restored.capabilities.pcm_analysis
                && !restored.capabilities.system_media_session
                && !restored.transport.queue_edit
        );
        assert_eq!(
            restored.queue.current.unwrap().id,
            saved_remote().current.id
        );
        assert!(restored.queue.upcoming.is_empty());
        assert_eq!(core.queue.current().unwrap().id, demo_tracks()[1].id);
        assert_eq!(core.position, 12_345);
        assert!(network.lock().unwrap().actions.is_empty());
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Visibility { visible: true },
        })
        .unwrap();
        let storage = storage.lock().unwrap();
        assert_eq!(
            storage
                .selections
                .last()
                .unwrap()
                .as_ref()
                .unwrap()
                .position_ms,
            42_000
        );
        assert!(
            storage.saves.is_empty(),
            "Remote position must not clone the local archive"
        );
    }
}
#[test]
fn authenticated_legacy_selection_promotes_and_survives_a_profile_id_change() {
    for profile_first in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        if profile_first {
            bind_profile_identity(
                &mut core,
                &network,
                "original-account-a",
                Some("immutable-a"),
            );
        }
        load_selection(&mut core, &storage);
        if !profile_first {
            assert!(core.snapshot().restored_remote.is_none());
            bind_profile_identity(
                &mut core,
                &network,
                "original-account-a",
                Some("immutable-a"),
            );
        }
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Visibility { visible: true },
        })
        .unwrap();
        let promoted = storage
            .lock()
            .unwrap()
            .selections
            .last()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(promoted.account_id, "original-account-a");
        assert_eq!(promoted.stable_account_id.as_deref(), Some("immutable-a"));

        let (mut restarted, network, storage) = session_restore_setup();
        load_remote_selection(&mut restarted, &storage, promoted);
        bind_profile_identity(
            &mut restarted,
            &network,
            "renamed-profile-a",
            Some("immutable-a"),
        );
        let snapshot = restarted.snapshot();
        assert!(snapshot.restored_remote.is_some());
        assert_eq!(snapshot.position_ms, 42_000);
        assert_eq!(snapshot.state, PlaybackState::Paused);
        assert!(!snapshot.playback_session_active);
        assert!(network.lock().unwrap().actions.is_empty());
    }
}
#[test]
fn immutable_account_restore_waits_for_evidence_and_rejects_a_legacy_key_collision() {
    let (mut core, network, storage) = session_restore_setup();
    let mut selected = saved_remote();
    selected.stable_account_id = Some("immutable-a".into());
    load_remote_selection(&mut core, &storage, selected.clone());

    bind_profile(&mut core, &network, "original-account-a");
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.remote_selection_for_storage(), Some(selected));
    adopt_output(&mut core);
    assert!(network.lock().unwrap().actions.is_empty());

    bind_profile_identity(&mut core, &network, "immutable-a", Some("immutable-b"));
    assert!(core.snapshot().restored_remote.is_none());
    assert!(core.remote_selection_for_storage().is_none());
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
    assert!(network.lock().unwrap().actions.is_empty());
}
#[test]
fn missing_immutable_id_keeps_the_binding_only_for_the_same_profile_and_credential() {
    for (replace_credential, profile) in [
        (false, "original-account-a"),
        (false, "renamed-profile"),
        (true, "original-account-a"),
    ] {
        let (mut core, network, storage) = session_restore_setup();
        storage
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::Loaded {
                session: None,
                issues: vec![],
            });
        core.tick(Duration::ZERO);
        bind_profile_identity(
            &mut core,
            &network,
            "original-account-a",
            Some("immutable-a"),
        );
        update(&mut core, &network, output_state(1, "owned", 42_000, false));
        adopt_output(&mut core);
        let before = core.remote_selection_for_storage().unwrap();
        assert_eq!(before.stable_account_id.as_deref(), Some("immutable-a"));

        if replace_credential {
            core.send(PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::Credential {
                    access_token: "updated-fixture-credential".into(),
                },
            })
            .unwrap();
        }
        bind_profile(&mut core, &network, profile);
        update(&mut core, &network, output_state(2, "owned", 46_000, false));
        core.send(PlayerCommand::Play).unwrap();
        assert!(matches!(
            network.lock().unwrap().actions.last(),
            Some(RemoteAction::Control {
                control: RemoteControl::Play
            })
        ));
        let retained = core.remote_selection_for_storage().unwrap();
        if !replace_credential && profile == "original-account-a" {
            assert_eq!(retained.stable_account_id, before.stable_account_id);
            assert_eq!(retained.position_ms, 46_000);
        } else {
            assert_eq!(retained, before);
        }

        bind_profile_identity(&mut core, &network, profile, Some("immutable-b"));
        let fresh = core.remote_selection_for_storage().unwrap();
        assert_eq!(fresh.stable_account_id.as_deref(), Some("immutable-b"));
        assert_eq!(fresh.position_ms, 46_000);
        assert!(core.snapshot().restored_remote.is_none());
    }
}
#[test]
fn saved_remote_data_does_not_cross_account_or_override_an_early_local_intent() {
    for local_intent in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        load_selection(&mut core, &storage);
        if local_intent {
            core.send(PlayerCommand::Play).unwrap();
        }
        bind_profile(
            &mut core,
            &network,
            if local_intent {
                "original-account-a"
            } else {
                "original-account-b"
            },
        );
        assert!(core.snapshot().restored_remote.is_none());
        assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Visibility { visible: true },
        })
        .unwrap();
        assert!(
            storage
                .lock()
                .unwrap()
                .saves
                .last()
                .unwrap()
                .remote
                .is_none()
        );
        assert!(
            !network
                .lock()
                .unwrap()
                .actions
                .iter()
                .any(|a| matches!(a, RemoteAction::Control { .. }))
        );
    }
}
#[test]
fn resume_requires_explicit_device_and_fresh_matching_playback_confirmation() {
    let (mut core, network, storage) = session_restore_setup();
    load_selection(&mut core, &storage);
    bind_profile(&mut core, &network, "original-account-a");
    update(&mut core, &network, output_state(1, "external", 1000, true));
    assert!(core.send(PlayerCommand::Play).is_err());
    request_saved_resume(&mut core, None);
    for frame in [
        output_state(1, "owned", 42_000, true),
        output_state(2, "external", 42_000, true),
        output_state(3, "owned", 1000, true),
        output_state(4, "owned", 42_000, false),
    ] {
        update(&mut core, &network, frame);
        assert!(core.snapshot().restored_remote.is_some());
        assert!(!core.snapshot().playback_session_active);
    }
    update(&mut core, &network, output_state(5, "owned", 42_500, true));
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().state, PlaybackState::Playing);
    assert!(core.snapshot().playback_session_active);
    assert_eq!(
        network
            .lock()
            .unwrap()
            .actions
            .iter()
            .filter(|a| matches!(
                a,
                RemoteAction::Control {
                    control: RemoteControl::ResumeOnDevice { .. }
                }
            ))
            .count(),
        1
    );
}
#[test]
fn tagged_resume_requires_current_acceptance_and_fresh_playback_in_either_order() {
    for acceptance_first in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        load_selection(&mut core, &storage);
        bind_profile(&mut core, &network, "original-account-a");
        update(&mut core, &network, output_state(1, "owned", 42_000, true));
        request_saved_resume(&mut core, Some(41));

        let mut accepted = output_state(2, "owned", 42_500, true);
        accepted.control_result = Some(RemoteControlResult {
            operation_id: 41,
            error: None,
        });
        if acceptance_first {
            // ACK-only delivery must survive later state messages with a
            // missing or unrelated result without granting a playback lease.
            accepted.observation_id = 1;
            accepted.playback = None;
            update(&mut core, &network, accepted);
            update(&mut core, &network, output_state(1, "owned", 42_000, true));
            let mut wrong_device = output_state(2, "external", 42_500, true);
            wrong_device.control_result = Some(RemoteControlResult {
                operation_id: 40,
                error: Some(PlayerError::invalid("Original obsolete rejection")),
            });
            update(&mut core, &network, wrong_device);
            assert!(core.snapshot().restored_remote.is_some());
            assert!(!core.snapshot().playback_session_active);
            update(&mut core, &network, output_state(3, "owned", 42_500, true));
        } else {
            update(&mut core, &network, output_state(2, "owned", 42_500, true));
            assert!(core.snapshot().restored_remote.is_some());
            assert!(!core.snapshot().playback_session_active);
            update(&mut core, &network, accepted);
        }
        assert!(core.snapshot().restored_remote.is_none());
        assert_eq!(core.snapshot().state, PlaybackState::Playing);
        assert!(core.snapshot().playback_session_active);
    }
}

fn confirmed_resume_for_scoped_pause() -> (PlaybackCoordinator, Arc<Mutex<FixtureMailbox>>) {
    let (mut core, network, storage) = session_restore_setup();
    load_selection(&mut core, &storage);
    bind_profile(&mut core, &network, "original-account-a");
    update(&mut core, &network, output_state(1, "owned", 42_000, false));
    request_saved_resume(&mut core, Some(41));
    let mut confirmed = output_state(2, "owned", 42_500, true);
    confirmed.owned_output_observed = true;
    confirmed.owned_output_control = true;
    confirmed.control_result = Some(RemoteControlResult {
        operation_id: 41,
        error: None,
    });
    update(&mut core, &network, confirmed);
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().state, PlaybackState::Playing);
    assert!(core.remote.confirmed_resume_pause.is_some());
    network.lock().unwrap().actions.clear();
    while core.pop_event().is_some() {}
    (core, network)
}

fn scoped_resume_pause(operation_id: u64, device_id: &str) -> PlayerCommand {
    PlayerCommand::PauseConfirmedRestoredResume {
        operation_id,
        device_id: device_id.into(),
    }
}

#[test]
fn scoped_restored_pause_dispatches_native_once_after_exact_request_receipt() {
    let (mut core, network) = confirmed_resume_for_scoped_pause();
    let request_id = 52;
    {
        let mut network = network.lock().unwrap();
        network.accept_native_resume_pause = true;
        network.ready_play_pause = Some(request_id);
        network
            .updates
            .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                observed_queue_epoch: None,
                required_queue_epoch: None,
                expected_previous_media_id: None,
                id: 7,
                request_id: Some(request_id),
                generation: 2,
                sequence: 5,
                device_id: "owned".into(),
                media_id: saved_remote().current.id,
                expires_at_uptime_ms: 1_000,
                control: RemoteOutputControl::Pause,
            }));
    }

    for (wrong_request, command) in [
        (50, scoped_resume_pause(40, "owned")),
        (51, scoped_resume_pause(41, "phone")),
    ] {
        core.send_request(CommandRequest {
            request_id: wrong_request,
            command,
        });
        assert!(
            std::iter::from_fn(|| core.pop_event()).any(|event| matches!(
                event,
                PlayerEvent::CommandResult { request_id, error: Some(_) }
                    if request_id == wrong_request
            ))
        );
    }
    {
        let network = network.lock().unwrap();
        assert!(network.resume_pause_requests.is_empty());
        assert!(network.actions.is_empty());
        assert_eq!(network.updates.len(), 1);
    }
    assert!(core.remote.confirmed_resume_pause.is_some());
    while core.pop_event().is_some() {}

    core.send_request(CommandRequest {
        request_id,
        command: scoped_resume_pause(41, "owned"),
    });
    let events = std::iter::from_fn(|| core.pop_event())
        .filter_map(|event| match event {
            PlayerEvent::CommandResult {
                request_id: id,
                error: None,
            } if id == request_id => Some("receipt"),
            PlayerEvent::Snapshot { .. } => Some("snapshot"),
            PlayerEvent::RemoteOutputCommand { command, .. }
                if command.request_id == Some(request_id)
                    && command.control == RemoteOutputControl::Pause =>
            {
                Some("native pause")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(events, ["receipt", "snapshot", "native pause"]);
    assert!(core.send(scoped_resume_pause(41, "owned")).is_err());
    let network = network.lock().unwrap();
    assert_eq!(
        network.resume_pause_requests,
        [(41, "owned".into(), request_id)]
    );
    assert!(network.actions.is_empty());
    assert!(network.updates.is_empty());
}

#[test]
fn scoped_restored_pause_falls_back_once_to_the_exact_device() {
    for with_request_id in [true, false] {
        let (mut core, network) = confirmed_resume_for_scoped_pause();
        network.lock().unwrap().accept_native_resume_pause = !with_request_id;
        let command = scoped_resume_pause(41, "owned");
        if with_request_id {
            core.send_request(CommandRequest {
                request_id: 52,
                command: command.clone(),
            });
            assert!(
                std::iter::from_fn(|| core.pop_event()).any(|event| matches!(
                    event,
                    PlayerEvent::CommandResult {
                        request_id: 52,
                        error: None
                    }
                ))
            );
        } else {
            core.send(command.clone()).unwrap();
        }
        assert!(core.send(command).is_err());
        let network = network.lock().unwrap();
        if with_request_id {
            assert_eq!(network.resume_pause_requests, [(41, "owned".into(), 52)]);
        } else {
            assert!(network.resume_pause_requests.is_empty());
        }
        assert!(matches!(
            network.actions.as_slice(),
            [RemoteAction::Control { control: RemoteControl::PauseOnDevice { device_id } }]
                if device_id == "owned"
        ));
    }
}

#[test]
fn scoped_restored_pause_does_not_reacquire_a_changed_target() {
    let mut changed_context = output_state(3, "owned", 42_500, true);
    changed_context.playback.as_mut().unwrap().context_uri = Some("fixture:album:other".into());
    let mut changed_item = output_state(3, "owned", 42_500, true);
    changed_item
        .playback
        .as_mut()
        .unwrap()
        .current
        .as_mut()
        .unwrap()
        .id = MediaId::parse("fixture:track:other").unwrap();
    for changed in [
        output_state(3, "phone", 42_500, true),
        output_state(3, "owned", 60_000, true),
        changed_context,
        changed_item,
    ] {
        let (mut core, network) = confirmed_resume_for_scoped_pause();
        update(&mut core, &network, changed);
        update(&mut core, &network, output_state(4, "owned", 42_500, true));
        assert!(core.remote.confirmed_resume_pause.is_none());
        assert!(core.send(scoped_resume_pause(41, "owned")).is_err());
        let network = network.lock().unwrap();
        assert!(network.resume_pause_requests.is_empty());
        assert!(network.actions.is_empty());
    }
}

#[test]
fn rejected_resume_keeps_restore_and_late_acceptance_cannot_confirm_its_replacement() {
    let (mut core, network, storage) = session_restore_setup();
    load_selection(&mut core, &storage);
    bind_profile(&mut core, &network, "original-account-a");
    update(&mut core, &network, output_state(1, "owned", 42_000, false));
    request_saved_resume(&mut core, Some(41));
    let mut rejected = output_state(2, "owned", 42_500, true);
    rejected.control_result = Some(RemoteControlResult {
        operation_id: 41,
        error: Some(PlayerError::invalid("Original Resume rejection")),
    });
    update(&mut core, &network, rejected);
    assert!(core.remote.resume_pending.is_none());
    assert!(core.snapshot().restored_remote.is_some());
    assert!(!core.snapshot().playback_session_active);

    request_saved_resume(&mut core, Some(42));
    let mut matching = output_state(3, "owned", 42_500, true);
    matching.control_result = Some(RemoteControlResult {
        operation_id: 41,
        error: None,
    });
    update(&mut core, &network, matching.clone());
    assert!(core.snapshot().restored_remote.is_some());
    assert!(!core.snapshot().playback_session_active);
    matching.control_result.as_mut().unwrap().operation_id = 42;
    update(&mut core, &network, matching);
    assert!(core.snapshot().restored_remote.is_none());
    assert!(core.snapshot().playback_session_active);
}
#[test]
fn credential_refresh_hides_saved_metadata_until_the_account_is_verified_again() {
    let (mut core, network, storage) = session_restore_setup();
    load_selection(&mut core, &storage);
    bind_profile(&mut core, &network, "original-account-a");
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Credential {
            access_token: "original-fixture".into(),
        },
    })
    .unwrap();
    assert!(core.snapshot().restored_remote.is_none());
    assert_ne!(
        core.snapshot().queue.current.unwrap().id,
        saved_remote().current.id
    );
    bind_profile(&mut core, &network, "original-account-b");
    assert!(core.snapshot().restored_remote.is_none());
    assert!(core.remote.candidate.is_none());
}
#[test]
fn early_remote_activation_preserves_the_local_archive_loaded_after_it() {
    let (mut core, network, storage) = session_restore_setup();
    update(&mut core, &network, output_state(1, "owned", 1000, true));
    bind_profile(&mut core, &network, "original-account-a");
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Activate,
    })
    .unwrap();
    load_selection(&mut core, &storage);
    assert_eq!(core.queue.current().unwrap().id, demo_tracks()[1].id);
    assert_eq!(core.position, 12_345);
    assert!(core.snapshot().restored_remote.is_none());
    assert_eq!(core.snapshot().state, PlaybackState::Playing);
}
#[test]
fn startup_adoption_does_not_supersede_candidate_restored_or_resuming_selection() {
    for profile_first in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        update(&mut core, &network, output_state(1, "owned", 1000, true));
        if profile_first {
            bind_profile(&mut core, &network, "original-account-a");
            // The account is known but the disk read has not finished.
            adopt_output(&mut core);
            load_selection(&mut core, &storage);
        } else {
            load_selection(&mut core, &storage);
            // A disk candidate must survive until the account is verified.
            adopt_output(&mut core);
            assert!(core.remote.candidate.is_some());
            bind_profile(&mut core, &network, "original-account-a");
        }
        // The host may see the account page before the coalesced restored snapshot.
        adopt_output(&mut core);
        assert!(network.lock().unwrap().actions.is_empty());
        assert!(core.snapshot().restored_remote.is_some());
        assert_eq!(core.snapshot().position_ms, 42_000);
        assert!(!core.snapshot().playback_session_active);

        request_saved_resume(&mut core, Some(41));
        let mut receipt = output_state(1, "owned", 1000, true);
        receipt.control_result = Some(RemoteControlResult {
            operation_id: 41,
            error: None,
        });
        update(&mut core, &network, receipt);
        adopt_output(&mut core);
        assert_eq!(network.lock().unwrap().actions.len(), 1);
        assert!(core.snapshot().restored_remote.is_some());
        update(&mut core, &network, output_state(2, "owned", 42_500, true));
        assert!(core.snapshot().restored_remote.is_none());
        assert!(core.snapshot().playback_session_active);
    }
}
#[test]
fn startup_adoption_requires_verified_account_and_preserves_an_explicit_local_choice() {
    for local_choice in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        storage
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::Loaded {
                session: None,
                issues: vec![],
            });
        core.tick(Duration::ZERO);
        update(&mut core, &network, output_state(1, "owned", 1000, true));
        adopt_output(&mut core);
        assert!(network.lock().unwrap().actions.is_empty());

        bind_profile(&mut core, &network, "original-account-a");
        if local_choice {
            core.send(local_selection()).unwrap();
        }
        adopt_output(&mut core);
        let snapshot = core.snapshot();
        assert_eq!(snapshot.state, PlaybackState::Playing);
        if local_choice {
            assert!(network.lock().unwrap().actions.is_empty());
            assert_eq!(snapshot.active_provider.as_deref(), Some("mock"));
        } else {
            assert!(matches!(
                network.lock().unwrap().actions.as_slice(),
                [RemoteAction::Activate]
            ));
            assert_eq!(snapshot.active_provider.as_deref(), Some("fixture"));
            assert_eq!(
                snapshot.queue.current.unwrap().id,
                saved_remote().current.id
            );
            assert!(snapshot.position_ms >= 1000);
        }
    }
}
#[test]
fn a_local_selection_during_resume_waits_for_the_target_output_without_pausing_a_phone() {
    let (mut core, network, storage) = session_restore_setup();
    load_selection(&mut core, &storage);
    bind_profile(&mut core, &network, "original-account-a");
    update(&mut core, &network, output_state(1, "external", 1000, true));
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::ResumeOnDevice {
                operation_id: None,
                item: saved_remote().current,
                device_id: "owned".into(),
                position_ms: 42_000,
                context_uri: saved_remote().context_uri,
            },
        },
    })
    .unwrap();
    core.send(local_selection()).unwrap();
    assert!(!core.provider_loaded);
    assert!(core.remote.pending_local.is_some());
    assert!(matches!(
        network.lock().unwrap().actions.last(),
        Some(RemoteAction::Control {
            control: RemoteControl::PauseOnDevice { device_id }
        }) if device_id == "owned"
    ));
    update(&mut core, &network, output_state(2, "external", 2000, true));
    assert!(core.provider_loaded);
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
    assert!(core.snapshot().restored_remote.is_none());
}

#[test]
fn an_early_owned_resume_frame_respects_a_targeted_pause_or_local_selection() {
    for local_switch in [false, true] {
        let (mut core, network, storage) = session_restore_setup();
        let selected = saved_remote();
        load_selection(&mut core, &storage);
        bind_profile(&mut core, &network, "original-account-a");

        let mut discovery = output_state(1, "owned", selected.position_ms, false);
        let mut device = discovery.playback.take().unwrap().device.unwrap();
        device.is_active = false;
        discovery.devices.push(device);
        update(&mut core, &network, discovery.clone());
        request_saved_resume(&mut core, Some(41));
        discovery.control_result = Some(RemoteControlResult {
            operation_id: 41,
            error: None,
        });
        update(&mut core, &network, discovery);
        assert!(core.snapshot().restored_remote.is_some());
        assert!(!core.snapshot().playback_session_active);
        assert!(core.send(PlayerCommand::Next).is_err());

        core.send(if local_switch {
            local_selection()
        } else {
            PlayerCommand::Pause
        })
        .unwrap();
        assert!(matches!(network.lock().unwrap().actions.as_slice(), [
            RemoteAction::Control { control: RemoteControl::ResumeOnDevice { operation_id: Some(41), .. } },
            RemoteAction::Control { control: RemoteControl::PauseOnDevice { device_id } },
        ] if device_id == "owned"));

        let mut early = output_state(2, "owned", selected.position_ms + 10, true);
        early.owned_output_control = true;
        early.owned_output_observed = true;
        early.control_result = Some(RemoteControlResult {
            operation_id: 41,
            error: None,
        });
        early.playback.as_mut().unwrap().context_uri = selected.context_uri.clone();
        update(&mut core, &network, early.clone());
        assert!(core.snapshot().restored_remote.is_some());
        assert!(!core.snapshot().playback_session_active);
        assert!(!core.provider_loaded);
        assert_eq!(core.has_pending_local_switch(), local_switch);

        early.observation_id += 1;
        let paused = early.playback.as_mut().unwrap();
        paused.state = PlaybackState::Paused;
        paused.position_rate = 0.0;
        update(&mut core, &network, early);
        if local_switch {
            assert!(core.provider_loaded);
            assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
        } else {
            assert!(core.snapshot().restored_remote.is_some());
            assert_eq!(core.snapshot().position_ms, selected.position_ms);
            assert!(!core.snapshot().playback_session_active);
        }
    }
}

#[test]
fn only_an_exact_undispatched_resume_can_be_cancelled_without_pausing_a_device() {
    let (mut core, network, storage) = session_restore_setup();
    load_selection(&mut core, &storage);
    bind_profile(&mut core, &network, "original-account-a");
    let mut discovery = output_state(1, "owned", saved_remote().position_ms, false);
    let mut device = discovery.playback.take().unwrap().device.unwrap();
    device.is_active = false;
    discovery.devices.push(device);
    update(&mut core, &network, discovery);
    request_saved_resume(&mut core, Some(71));

    let cancel = |operation_id, device: &str| PlayerCommand::CancelRestoredResume {
        operation_id,
        device_id: device.into(),
    };
    assert!(core.send(cancel(70, "owned")).is_err());
    assert!(core.send(cancel(71, "phone")).is_err());
    assert!(
        network
            .lock()
            .unwrap()
            .resume_cancellation_attempts
            .is_empty()
    );

    network.lock().unwrap().can_cancel_resume = true;
    core.send(cancel(71, "owned")).unwrap();
    assert!(core.remote.resume_pending.is_none());
    assert_eq!(core.snapshot().state, PlaybackState::Paused);
    assert_eq!(core.snapshot().position_ms, saved_remote().position_ms);
    assert!(!core.snapshot().playback_session_active);

    request_saved_resume(&mut core, Some(72));
    assert!(core.send(cancel(71, "owned")).is_err());
    network.lock().unwrap().can_cancel_resume = false;
    assert!(core.send(cancel(72, "owned")).is_err());
    assert!(core.remote.resume_pending.is_some());
    let mailbox = network.lock().unwrap();
    assert_eq!(
        mailbox.resume_cancellation_attempts,
        vec![(71, "owned".into()), (72, "owned".into())]
    );
    assert_eq!(mailbox.actions.len(), 2);
}

#[test]
fn confirmed_remote_selection_survives_a_real_session_worker_restart_without_autoplay() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "auvra-remote-session-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fn open(directory: &std::path::Path) -> (PlaybackCoordinator, Arc<Mutex<FixtureMailbox>>) {
        let mut core = PlaybackCoordinator::default();
        core.register(MockProvider::default());
        core.register_session_store(player_session::SessionWorker::new().unwrap());
        let network = Arc::new(Mutex::new(FixtureMailbox::default()));
        core.register_remote(FixtureRemote(network.clone()));
        core.send(PlayerCommand::InitializeSession {
            directory: directory.to_str().unwrap().into(),
            access: SessionAccess::default(),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while !core.session_state.ready {
            core.tick(Duration::ZERO);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        (core, network)
    }
    {
        let (mut core, network) = open(&directory);
        core.send(PlayerCommand::ReplaceQueue {
            items: demo_tracks(),
            start: 1,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        core.tick(Duration::from_millis(500));
        core.send(PlayerCommand::Pause).unwrap();
        update(&mut core, &network, output_state(1, "owned", 42_000, true));
        bind_profile(&mut core, &network, "original-account-a");
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Activate,
        })
        .unwrap();
        core.shutdown();
    }
    {
        let (mut core, network) = open(&directory);
        let mut idle = output_state(2, "owned", 0, false);
        idle.playback = None;
        update(&mut core, &network, idle);
        bind_profile(&mut core, &network, "original-account-a");
        let snapshot = core.snapshot();
        assert!(snapshot.restored_remote.is_some());
        assert_eq!(snapshot.state, PlaybackState::Paused);
        assert!((42_000..43_000).contains(&snapshot.position_ms));
        assert!(!snapshot.playback_session_active);
        assert_eq!(core.queue.current().unwrap().id, demo_tracks()[1].id);
        assert_eq!(core.position, 500);
        assert!(network.lock().unwrap().actions.is_empty());
        core.shutdown();
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[derive(Default)]
struct FixtureMailbox {
    actions: Vec<RemoteAction>,
    updates: VecDeque<RemoteUpdate>,
    starting: bool,
    previous_after_next: Vec<String>,
    next_request_ids: Vec<u64>,
    ready_next: Option<u64>,
    previous_request_ids: Vec<u64>,
    seek_requests: Vec<(u64, u64)>,
    ready_seek: Option<u64>,
    play_pause_requests: Vec<(u64, bool)>,
    targeted_pause_requests: Vec<(u64, String)>,
    ready_play_pause: Option<u64>,
    can_cancel_resume: bool,
    resume_cancellation_attempts: Vec<(u64, String)>,
    accept_native_resume_pause: bool,
    resume_pause_requests: Vec<(u64, String, u64)>,
    previous_ready_after: Option<fn(&RemoteAction) -> bool>,
}
struct FixtureRemote(Arc<Mutex<FixtureMailbox>>);
impl RemoteProvider for FixtureRemote {
    fn id(&self) -> &str {
        "fixture"
    }
    fn submit(&mut self, action: RemoteAction) -> Result<(), PlayerError> {
        self.0.lock().unwrap().actions.push(action);
        Ok(())
    }
    fn submit_next_with_request_id(&mut self, request_id: u64) -> Result<(), PlayerError> {
        self.0.lock().unwrap().next_request_ids.push(request_id);
        self.submit(RemoteAction::Control {
            control: RemoteControl::Next,
        })
    }
    fn owned_next_ready_for_request(&self, request_id: u64) -> bool {
        self.0.lock().unwrap().ready_next == Some(request_id)
    }
    fn submit_previous_with_request_id(&mut self, request_id: u64) -> Result<(), PlayerError> {
        self.0.lock().unwrap().previous_request_ids.push(request_id);
        self.submit(RemoteAction::Control {
            control: RemoteControl::Previous,
        })
    }
    fn submit_seek_with_request_id(
        &mut self,
        position_ms: u64,
        request_id: u64,
    ) -> Result<(), PlayerError> {
        self.0
            .lock()
            .unwrap()
            .seek_requests
            .push((request_id, position_ms));
        self.submit(RemoteAction::Control {
            control: RemoteControl::Seek { position_ms },
        })
    }
    fn owned_seek_ready_for_request(&self, request_id: u64) -> bool {
        let mailbox = self.0.lock().unwrap();
        mailbox.ready_seek == Some(request_id)
            && mailbox
                .seek_requests
                .last()
                .is_some_and(|seek| seek.0 == request_id)
    }
    fn submit_play_pause_with_request_id(
        &mut self,
        play: bool,
        request_id: u64,
    ) -> Result<(), PlayerError> {
        self.0
            .lock()
            .unwrap()
            .play_pause_requests
            .push((request_id, play));
        self.submit(RemoteAction::Control {
            control: if play {
                RemoteControl::Play
            } else {
                RemoteControl::Pause
            },
        })
    }
    fn owned_play_pause_ready_for_request(&self, request_id: u64) -> bool {
        let mailbox = self.0.lock().unwrap();
        mailbox.ready_play_pause == Some(request_id)
            && (mailbox
                .play_pause_requests
                .last()
                .is_some_and(|intent| intent.0 == request_id)
                || mailbox
                    .targeted_pause_requests
                    .last()
                    .is_some_and(|intent| intent.0 == request_id)
                || (mailbox.accept_native_resume_pause
                    && mailbox
                        .resume_pause_requests
                        .last()
                        .is_some_and(|intent| intent.2 == request_id)))
    }
    fn submit_pause_on_device_with_request_id(
        &mut self,
        device_id: &str,
        request_id: u64,
    ) -> Result<(), PlayerError> {
        self.0
            .lock()
            .unwrap()
            .targeted_pause_requests
            .push((request_id, device_id.into()));
        self.submit(RemoteAction::Control {
            control: RemoteControl::PauseOnDevice {
                device_id: device_id.into(),
            },
        })
    }
    fn submit_owned_pause_after_resume(
        &mut self,
        operation_id: u64,
        device_id: &str,
        request_id: u64,
    ) -> Result<bool, PlayerError> {
        let mut mailbox = self.0.lock().unwrap();
        mailbox
            .resume_pause_requests
            .push((operation_id, device_id.into(), request_id));
        Ok(mailbox.accept_native_resume_pause)
    }
    fn owned_handoff_ready(&self) -> bool {
        let mailbox = self.0.lock().unwrap();
        mailbox
            .previous_ready_after
            .is_some_and(|qualifies| mailbox.actions.last().is_some_and(qualifies))
    }
    fn cancel_resume_before_dispatch(&mut self, operation_id: u64, device_id: &str) -> bool {
        let mut mailbox = self.0.lock().unwrap();
        mailbox
            .resume_cancellation_attempts
            .push((operation_id, device_id.into()));
        mailbox.can_cancel_resume
    }
    fn poll(&mut self) -> Option<RemoteUpdate> {
        self.0.lock().unwrap().updates.pop_front()
    }
    fn submit_previous_after_next(&mut self, device_id: &str) -> Result<(), PlayerError> {
        self.0
            .lock()
            .unwrap()
            .previous_after_next
            .push(device_id.into());
        Ok(())
    }
    fn has_pending_output_start(&self) -> bool {
        self.0.lock().unwrap().starting
    }
}
fn state(observation_id: u64, playing: bool) -> RemoteState {
    let mut item = demo_tracks().remove(0);
    item.id = MediaId::parse("fixture:track:remote").unwrap();
    item.capabilities = Capabilities::REMOTE;
    RemoteState {
        owned_output_observed: false,
        owned_output_control: false,
        control_result: None,
        confirmed_same_media_next_request_id: None,
        observation_id,
        provider: "fixture".into(),
        connection: ProviderConnectionState::Connected,
        authentication_required: false,
        issue: None,
        retry_after_seconds: None,
        devices: vec![],
        playback: Some(RemotePlayback {
            enhancement: None,
            context_uri: None,
            state: if playing {
                PlaybackState::Playing
            } else {
                PlaybackState::Paused
            },
            position_ms: 1000,
            position_rate: 1.0,
            volume: 0.3,
            current: Some(item),
            upcoming: vec![],
            device_queue: None,
            shuffle: true,
            repeat: RepeatMode::All,
            capabilities: Capabilities::REMOTE,
            transport: TransportControls {
                queue_add: true,
                next: true,
                previous: true,
                shuffle: true,
                repeat: true,
                ..Default::default()
            },
            device: None,
        }),
    }
}
fn coordinator() -> (PlaybackCoordinator, Arc<Mutex<FixtureMailbox>>) {
    let mut core = PlaybackCoordinator::default();
    core.register(MockProvider::default());
    core.send(PlayerCommand::ReplaceQueue {
        items: demo_tracks(),
        start: 0,
        context: PlaybackContext::UserQueue,
    })
    .unwrap();
    core.tick(Duration::from_millis(500));
    let mailbox = Arc::new(Mutex::new(FixtureMailbox::default()));
    core.register_remote(FixtureRemote(mailbox.clone()));
    update(&mut core, &mailbox, state(1, true));
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Activate,
    })
    .unwrap();
    (core, mailbox)
}
fn update(
    core: &mut PlaybackCoordinator,
    mailbox: &Arc<Mutex<FixtureMailbox>>,
    state: RemoteState,
) {
    mailbox
        .lock()
        .unwrap()
        .updates
        .push_back(RemoteUpdate::State(Box::new(state)));
    core.tick(Duration::ZERO);
}
fn local_selection() -> PlayerCommand {
    PlayerCommand::ReplaceQueue {
        items: demo_tracks(),
        start: 1,
        context: PlaybackContext::UserQueue,
    }
}
#[test]
fn same_media_next_keeps_only_the_admitted_client_next_request_id() {
    let (mut core, mailbox) = coordinator();
    core.send_request(CommandRequest {
        request_id: 40,
        command: PlayerCommand::Next,
    });
    core.send(PlayerCommand::Next).unwrap();
    core.send_request(CommandRequest {
        request_id: 41,
        command: PlayerCommand::Pause,
    });
    let mut restricted = state(2, true);
    restricted.playback.as_mut().unwrap().transport.next = false;
    update(&mut core, &mailbox, restricted);
    core.send_request(CommandRequest {
        request_id: 42,
        command: PlayerCommand::Next,
    });

    assert_eq!(mailbox.lock().unwrap().next_request_ids, [40]);
    assert!(
        std::iter::from_fn(|| core.pop_event()).any(|event| matches!(event,
            PlayerEvent::CommandResult { request_id: 42, error: Some(error) }
                if error.code == ErrorCode::UnsupportedCapability
        ))
    );
}

#[test]
fn owned_next_ready_is_drained_after_receipt_and_snapshot_only_for_the_exact_request() {
    let (mut core, mailbox) = coordinator();
    while core.pop_event().is_some() {}
    let request_id = 42;
    let mut owned = state(2, true);
    owned.owned_output_observed = true;
    owned.owned_output_control = true;
    let media = owned
        .playback
        .as_ref()
        .unwrap()
        .current
        .as_ref()
        .unwrap()
        .id
        .clone();
    {
        let mut mailbox = mailbox.lock().unwrap();
        mailbox.ready_next = Some(request_id);
        mailbox
            .updates
            .push_back(RemoteUpdate::State(Box::new(owned)));
        mailbox
            .updates
            .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                observed_queue_epoch: None,
                required_queue_epoch: None,
                expected_previous_media_id: None,
                id: 7,
                request_id: Some(request_id),
                generation: 2,
                sequence: 5,
                device_id: "owned".into(),
                media_id: media,
                expires_at_uptime_ms: 1_000,
                control: RemoteOutputControl::Next,
            }));
    }

    core.send_request(CommandRequest {
        request_id: request_id - 1,
        command: PlayerCommand::Next,
    });
    assert_eq!(mailbox.lock().unwrap().updates.len(), 2);
    core.send_request(CommandRequest {
        request_id,
        command: PlayerCommand::Next,
    });
    assert!(mailbox.lock().unwrap().updates.is_empty());

    let kinds = std::iter::from_fn(|| core.pop_event())
        .filter_map(|event| match event {
            PlayerEvent::CommandResult {
                request_id: id,
                error: None,
            } if id == request_id => Some("receipt"),
            PlayerEvent::Snapshot { .. } => Some("snapshot"),
            PlayerEvent::RemoteOutputCommand { command, .. }
                if command.request_id == Some(request_id) =>
            {
                Some("output")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["receipt", "snapshot", "output"]);
}

#[test]
fn owned_seek_drains_only_the_ready_client_request_after_its_receipt() {
    let (mut core, mailbox) = coordinator();
    let request_id = 42;
    let mut owned = output_state(2, "owned", 4_000, false);
    owned.owned_output_observed = true;
    owned.owned_output_control = true;
    let item = owned.playback.as_ref().unwrap().current.as_ref().unwrap();
    let media = item.id.clone();
    let duration = item.duration_ms;
    update(&mut core, &mailbox, owned);
    while core.pop_event().is_some() {}
    {
        let mut mailbox = mailbox.lock().unwrap();
        mailbox.ready_seek = Some(request_id);
        mailbox
            .updates
            .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                observed_queue_epoch: None,
                required_queue_epoch: None,
                expected_previous_media_id: None,
                id: 7,
                request_id: Some(request_id),
                generation: 2,
                sequence: 5,
                device_id: "owned".into(),
                media_id: media,
                expires_at_uptime_ms: 1_000,
                control: RemoteOutputControl::Seek { position_ms: 0 },
            }));
    }

    core.send_request(CommandRequest {
        request_id: request_id - 1,
        command: PlayerCommand::Seek {
            position_ms: u64::MAX,
        },
    });
    assert_eq!(mailbox.lock().unwrap().updates.len(), 1);
    core.send_request(CommandRequest {
        request_id,
        command: PlayerCommand::Previous,
    });
    let mailbox = mailbox.lock().unwrap();
    assert!(mailbox.updates.is_empty());
    assert_eq!(
        mailbox.seek_requests,
        [(request_id - 1, duration), (request_id, 0)]
    );
    drop(mailbox);

    let mut exact_receipt_seen = false;
    let mut snapshot_after_receipt = false;
    let mut output_seen = false;
    for event in std::iter::from_fn(|| core.pop_event()) {
        match event {
            PlayerEvent::CommandResult {
                request_id: id,
                error: None,
            } if id == request_id => {
                exact_receipt_seen = true;
            }
            PlayerEvent::Snapshot { .. } if exact_receipt_seen => snapshot_after_receipt = true,
            PlayerEvent::RemoteOutputCommand { command, .. }
                if command.request_id == Some(request_id) =>
            {
                assert!(exact_receipt_seen && snapshot_after_receipt);
                output_seen = true;
            }
            _ => {}
        }
    }
    assert!(output_seen);
}

#[test]
fn owned_play_pause_wakes_only_exact_admitted_request_after_receipt() {
    for play in [true, false] {
        let (mut core, mailbox) = coordinator();
        let request_id = 42;
        let command = if play {
            PlayerCommand::Play
        } else {
            PlayerCommand::Pause
        };
        let mut owned = output_state(2, "owned", 4_000, !play);
        owned.owned_output_observed = true;
        owned.owned_output_control = true;
        let media = owned
            .playback
            .as_ref()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .id
            .clone();
        update(&mut core, &mailbox, owned);
        while core.pop_event().is_some() {}
        {
            let mut mailbox = mailbox.lock().unwrap();
            mailbox.ready_play_pause = Some(request_id);
            mailbox
                .updates
                .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                    observed_queue_epoch: None,
                    required_queue_epoch: None,
                    expected_previous_media_id: None,
                    id: 7,
                    request_id: Some(request_id),
                    generation: 2,
                    sequence: 5,
                    device_id: "owned".into(),
                    media_id: media,
                    expires_at_uptime_ms: 1_000,
                    control: if play {
                        RemoteOutputControl::Play
                    } else {
                        RemoteOutputControl::Pause
                    },
                }));
        }

        core.send_request(CommandRequest {
            request_id: request_id - 1,
            command: command.clone(),
        });
        assert_eq!(mailbox.lock().unwrap().updates.len(), 1);
        while core.pop_event().is_some() {}
        core.send_request(CommandRequest {
            request_id,
            command,
        });
        {
            let mailbox = mailbox.lock().unwrap();
            assert!(mailbox.updates.is_empty());
            assert_eq!(
                mailbox.play_pause_requests,
                [(request_id - 1, play), (request_id, play)]
            );
        }
        let sequence = std::iter::from_fn(|| core.pop_event())
            .filter_map(|event| match event {
                PlayerEvent::CommandResult {
                    request_id: id,
                    error: None,
                } if id == request_id => Some("receipt"),
                PlayerEvent::Snapshot { .. } => Some("snapshot"),
                PlayerEvent::RemoteOutputCommand { command, .. }
                    if command.request_id == Some(request_id) =>
                {
                    Some("output")
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(sequence, ["receipt", "snapshot", "output"]);
    }
}

#[test]
fn external_play_pause_are_submitted_without_owned_request_ids() {
    let (mut core, mailbox) = coordinator();
    mailbox.lock().unwrap().actions.clear();
    core.send_request(CommandRequest {
        request_id: 41,
        command: PlayerCommand::Pause,
    });
    core.send_request(CommandRequest {
        request_id: 42,
        command: PlayerCommand::Play,
    });
    let mailbox = mailbox.lock().unwrap();
    assert!(mailbox.play_pause_requests.is_empty());
    assert!(matches!(
        mailbox.actions.as_slice(),
        [
            RemoteAction::Control {
                control: RemoteControl::Pause
            },
            RemoteAction::Control {
                control: RemoteControl::Play
            },
        ]
    ));
}

#[test]
fn addressed_pause_preserves_target_and_wakes_only_its_exact_native_request_after_receipt() {
    let (mut core, mailbox) = coordinator();
    let request_id = 42;
    let mut owned = output_state(2, "owned", 4_000, true);
    owned.owned_output_observed = true;
    // A confirmed row may still have no generic Pause admission capability.
    owned.owned_output_control = false;
    let playback = owned.playback.as_mut().unwrap();
    playback.capabilities.pause = false;
    let media = playback.current.as_ref().unwrap().id.clone();
    update(&mut core, &mailbox, owned);
    while core.pop_event().is_some() {}
    {
        let mut mailbox = mailbox.lock().unwrap();
        mailbox.ready_play_pause = Some(request_id);
        mailbox
            .updates
            .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                observed_queue_epoch: None,
                required_queue_epoch: Some(7),
                expected_previous_media_id: None,
                id: 7,
                request_id: Some(request_id),
                generation: 2,
                sequence: 5,
                device_id: "owned".into(),
                media_id: media,
                expires_at_uptime_ms: 1_000,
                control: RemoteOutputControl::Pause,
            }));
    }
    let addressed = || PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::PauseOnDevice {
                device_id: "owned".into(),
            },
        },
    };
    core.send_request(CommandRequest {
        request_id: request_id - 1,
        command: addressed(),
    });
    assert_eq!(mailbox.lock().unwrap().updates.len(), 1);
    while core.pop_event().is_some() {}
    core.send_request(CommandRequest {
        request_id,
        command: addressed(),
    });
    {
        let mailbox = mailbox.lock().unwrap();
        assert!(mailbox.updates.is_empty());
        assert_eq!(
            mailbox.targeted_pause_requests,
            [
                (request_id - 1, "owned".into()),
                (request_id, "owned".into())
            ]
        );
        assert!(mailbox.play_pause_requests.is_empty());
        assert!(
            matches!(mailbox.actions.last(), Some(RemoteAction::Control {
            control: RemoteControl::PauseOnDevice { device_id },
        }) if device_id == "owned")
        );
    }
    let sequence = std::iter::from_fn(|| core.pop_event())
        .filter_map(|event| match event {
            PlayerEvent::CommandResult {
                request_id: id,
                error: None,
            } if id == request_id => Some("receipt"),
            PlayerEvent::Snapshot { .. } => Some("snapshot"),
            PlayerEvent::RemoteOutputCommand { command, .. }
                if command.request_id == Some(request_id) =>
            {
                Some("output")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sequence, ["receipt", "snapshot", "output"]);
}

#[test]
fn addressed_pause_can_wake_a_ready_active_handoff_without_losing_its_destination() {
    let (mut core, mailbox) = coordinator();
    while core.pop_event().is_some() {}
    let media = core.snapshot().queue.current.unwrap().id;
    {
        let mut mailbox = mailbox.lock().unwrap();
        mailbox.previous_ready_after = Some(|action| {
            matches!(action,
            RemoteAction::Control { control: RemoteControl::PauseOnDevice { device_id } } if device_id == "owned")
        });
        mailbox
            .updates
            .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                observed_queue_epoch: None,
                required_queue_epoch: Some(7),
                expected_previous_media_id: None,
                id: 8,
                request_id: Some(43),
                generation: 2,
                sequence: 6,
                device_id: "owned".into(),
                media_id: media,
                expires_at_uptime_ms: 1_000,
                control: RemoteOutputControl::Pause,
            }));
    }
    let command = PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::PauseOnDevice {
                device_id: "owned".into(),
            },
        },
    };
    assert!(core.may_confirm_owned_handoff(&command));
    assert!(!core.may_confirm_owned_handoff(&PlayerCommand::Remote {
        provider: "other".into(),
        action: RemoteAction::Control {
            control: RemoteControl::PauseOnDevice {
                device_id: "owned".into()
            },
        },
    }));
    core.send_request(CommandRequest {
        request_id: 43,
        command,
    });
    assert!(mailbox.lock().unwrap().updates.is_empty());
    let events: Vec<_> = std::iter::from_fn(|| core.pop_event()).collect();
    let receipt = events
        .iter()
        .position(|event| {
            matches!(
                event,
                PlayerEvent::CommandResult {
                    request_id: 43,
                    error: None
                }
            )
        })
        .unwrap();
    let output = events
        .iter()
        .position(|event| {
            matches!(event,
        PlayerEvent::RemoteOutputCommand { command, .. } if command.request_id == Some(43))
        })
        .unwrap();
    assert!(receipt < output);
}

#[test]
fn remote_seek_without_owned_scope_and_previous_before_four_seconds_stay_ordinary() {
    let (mut core, mailbox) = coordinator();
    mailbox.lock().unwrap().actions.clear();
    core.send_request(CommandRequest {
        request_id: 40,
        command: PlayerCommand::Seek { position_ms: 1_000 },
    });

    let mut owned = output_state(2, "owned", 3_999, false);
    owned.owned_output_observed = true;
    owned.owned_output_control = true;
    update(&mut core, &mailbox, owned);
    core.send_request(CommandRequest {
        request_id: 41,
        command: PlayerCommand::Previous,
    });
    core.send(PlayerCommand::Seek { position_ms: 2_000 })
        .unwrap();

    let mailbox = mailbox.lock().unwrap();
    assert!(mailbox.seek_requests.is_empty());
    assert!(matches!(
        mailbox.actions.as_slice(),
        [
            RemoteAction::Control {
                control: RemoteControl::Seek { position_ms: 1_000 }
            },
            RemoteAction::Control {
                control: RemoteControl::Previous
            },
            RemoteAction::Control {
                control: RemoteControl::Seek { position_ms: 2_000 }
            },
        ]
    ));
}

#[test]
fn owned_handoff_drains_after_the_final_confirmation_without_an_idle_tick() {
    for final_evidence in ["previous", "seek receipt", "seek clock", "next queue"] {
        let (mut core, mailbox) = coordinator();
        while core.pop_event().is_some() {}
        let mut owned = state(2, false);
        owned.owned_output_observed = true;
        owned.owned_output_control = true;
        let playback = owned.playback.as_mut().unwrap();
        playback.position_ms = 3500;
        playback.position_rate = 0.0;
        let media = playback.current.as_ref().unwrap().id.clone();
        let clock = RemoteOutputClock {
            generation: 2,
            sequence: 5,
            device_id: "owned".into(),
            media_id: Some(media.clone()),
            state: PlaybackState::Paused,
            position_ms: 3500,
            rate: 0.0,
            active: true,
            volume: None,
            options: None,
            actions: None,
            metadata: None,
            control_uptime_ms: Some(100),
            queue_epoch: Some(10),
            confirmed_same_media_next: None,
        };
        let input = match final_evidence {
            "previous" => PlayerCommand::SkipPrevious,
            "seek receipt" => PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::OutputCommandResult {
                    result: RemoteOutputCommandResult {
                        id: 7,
                        generation: 2,
                        accepted: true,
                    },
                },
            },
            "seek clock" => PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::OutputClock { clock: Some(clock) },
            },
            "next queue" => PlayerCommand::Remote {
                provider: "fixture".into(),
                action: RemoteAction::OutputQueue {
                    queue: RemoteOutputQueue {
                        generation: 2,
                        device_id: "owned".into(),
                        media_id: media.clone(),
                        epoch: 10,
                        sequence: 3,
                        autoplay_ready: false,
                        previous: Vec::new(),
                        upcoming: Vec::new(),
                    },
                },
            },
            _ => unreachable!(),
        };
        let control = if final_evidence == "next queue" {
            RemoteOutputControl::Next
        } else {
            RemoteOutputControl::PreviousFromStart
        };
        {
            let mut mailbox = mailbox.lock().unwrap();
            mailbox.previous_ready_after = Some(match final_evidence {
                "previous" => |action| {
                    matches!(
                        action,
                        RemoteAction::Control {
                            control: RemoteControl::Previous
                        }
                    )
                },
                "seek receipt" => |action| {
                    matches!(
                        action,
                        RemoteAction::OutputCommandResult {
                            result: RemoteOutputCommandResult {
                                id: 7,
                                accepted: true,
                                ..
                            }
                        }
                    )
                },
                "seek clock" => {
                    |action| matches!(action, RemoteAction::OutputClock { clock: Some(clock) } if clock.sequence == 5)
                }
                "next queue" => {
                    |action| matches!(action, RemoteAction::OutputQueue { queue } if queue.sequence == 3)
                }
                _ => unreachable!(),
            });
            mailbox
                .updates
                .push_back(RemoteUpdate::State(Box::new(owned)));
            mailbox
                .updates
                .push_back(RemoteUpdate::OutputCommand(RemoteOutputCommand {
                    id: 8,
                    request_id: None,
                    required_queue_epoch: Some(10),
                    observed_queue_epoch: None,
                    expected_previous_media_id: None,
                    generation: 2,
                    sequence: 5,
                    device_id: "owned".into(),
                    media_id: media,
                    expires_at_uptime_ms: 1000,
                    control: control.clone(),
                }));
        }
        core.send_request(CommandRequest {
            request_id: 41,
            command: PlayerCommand::Pause,
        });
        assert_eq!(mailbox.lock().unwrap().updates.len(), 2);
        core.send_request(CommandRequest {
            request_id: 42,
            command: input,
        });
        assert!(
            mailbox.lock().unwrap().updates.is_empty(),
            "{final_evidence}"
        );
        let kinds = std::iter::from_fn(|| core.pop_event())
            .filter_map(|event| match event {
                PlayerEvent::CommandResult {
                    request_id: 42,
                    error: None,
                } => Some("receipt"),
                PlayerEvent::Snapshot { .. } => Some("snapshot"),
                PlayerEvent::RemoteOutputCommand { command, .. } if command.control == control => {
                    Some("output")
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["receipt", "snapshot", "output"], "{final_evidence}");
    }
}

#[test]
fn remote_previous_switches_before_four_seconds_and_restarts_at_four() {
    let (mut core, mailbox) = coordinator();
    for (observation, position_ms) in [(2, 3999), (3, 4000)] {
        let mut output = state(observation, false);
        output.playback.as_mut().unwrap().position_ms = position_ms;
        update(&mut core, &mailbox, output);
        core.send(PlayerCommand::Previous).unwrap();
        assert!(matches!(
            (position_ms, mailbox.lock().unwrap().actions.last()),
            (
                3999,
                Some(RemoteAction::Control {
                    control: RemoteControl::Previous
                })
            ) | (
                4000,
                Some(RemoteAction::Control {
                    control: RemoteControl::Seek { position_ms: 0 }
                })
            )
        ));
    }
}

#[test]
fn previous_request_id_is_bound_only_to_admitted_owned_previous() {
    let (mut core, mailbox) = coordinator();
    for (request_id, owned, can_skip, position_ms, command) in [
        (40, false, true, 1_000, PlayerCommand::Previous),
        (41, true, true, 3_999, PlayerCommand::Previous),
        (42, true, true, 4_000, PlayerCommand::Previous),
        (43, true, true, 4_500, PlayerCommand::SkipPrevious),
        (44, true, false, 1_000, PlayerCommand::SkipPrevious),
        (45, true, false, 1_000, PlayerCommand::Previous),
    ] {
        let mut output = output_state(request_id, "owned", position_ms, false);
        output.owned_output_observed = owned;
        output.owned_output_control = owned;
        output.playback.as_mut().unwrap().transport.previous = can_skip;
        update(&mut core, &mailbox, output);
        core.send_request(CommandRequest {
            request_id,
            command,
        });
    }
    let mailbox = mailbox.lock().unwrap();
    assert_eq!(mailbox.previous_request_ids, [41, 43]);
    assert_eq!(mailbox.seek_requests, [(42, 0)]);
    assert_eq!(mailbox.previous_after_next, ["owned"]);
    assert!(
        std::iter::from_fn(|| core.pop_event()).any(|event| matches!(event,
            PlayerEvent::CommandResult { request_id: 45, error: Some(error) }
                if error.code == ErrorCode::UnsupportedCapability
        ))
    );
}

#[test]
fn remote_explicit_previous_keeps_click_intent_after_crossing_threshold() {
    let (mut core, mailbox) = coordinator();
    let mut output = state(2, false);
    output.playback.as_mut().unwrap().position_ms = 4001;
    update(&mut core, &mailbox, output);
    let command = serde_json::from_str::<PlayerCommand>(r#"{"type":"skip_previous"}"#).unwrap();
    core.send(command.clone()).unwrap();
    assert!(matches!(
        mailbox.lock().unwrap().actions.last(),
        Some(RemoteAction::Control {
            control: RemoteControl::Previous
        })
    ));

    let mut restricted = state(3, false);
    let playback = restricted.playback.as_mut().unwrap();
    playback.position_ms = 4001;
    playback.transport.previous = false;
    update(&mut core, &mailbox, restricted);
    let action_count = mailbox.lock().unwrap().actions.len();
    assert_eq!(
        core.send(command).unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
    assert_eq!(mailbox.lock().unwrap().actions.len(), action_count);
}
#[test]
fn explicit_previous_during_owned_navigation_uses_the_active_device() {
    let (mut core, mailbox) = coordinator();
    let mut frame = output_state(2, "owned", 1_000, true);
    frame.playback.as_mut().unwrap().transport.previous = false;
    update(&mut core, &mailbox, frame.clone());
    assert_eq!(
        core.send(PlayerCommand::SkipPrevious).unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
    frame.observation_id += 1;
    frame.owned_output_control = true;
    update(&mut core, &mailbox, frame.clone());
    core.send(PlayerCommand::SkipPrevious).unwrap();
    assert_eq!(mailbox.lock().unwrap().previous_after_next, ["owned"]);

    frame.observation_id += 1;
    frame
        .playback
        .as_mut()
        .unwrap()
        .device
        .as_mut()
        .unwrap()
        .is_restricted = true;
    update(&mut core, &mailbox, frame);
    assert_eq!(
        core.send(PlayerCommand::SkipPrevious).unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
    assert_eq!(mailbox.lock().unwrap().previous_after_next, ["owned"]);
}
#[test]
fn remote_owns_queue_modes_eof_and_does_not_replace_local_archive() {
    let (mut core, mailbox) = coordinator();
    let local = core.queue.snapshot();
    let remote = core.snapshot();
    assert_eq!(remote.active_provider.as_deref(), Some("fixture"));
    assert!(!remote.capabilities.pcm_analysis);
    assert!(!remote.capabilities.system_media_session);
    assert!(!remote.transport.queue_edit);
    core.tick(Duration::from_secs(86400));
    assert_eq!(
        core.queue.snapshot().current.unwrap().id,
        local.current.unwrap().id
    );
    core.send(PlayerCommand::Next).unwrap();
    core.send(PlayerCommand::SetRepeat {
        mode: RepeatMode::One,
    })
    .unwrap();
    assert!(core.send(PlayerCommand::ClearQueue).is_err());
    assert_eq!(core.snapshot().queue.repeat, RepeatMode::All);
    assert!(mailbox.lock().unwrap().actions.iter().any(|a| matches!(
        a,
        RemoteAction::Control {
            control: RemoteControl::Next
        }
    )));
    let mut external = state(2, false);
    external
        .playback
        .as_mut()
        .unwrap()
        .current
        .as_mut()
        .unwrap()
        .title = "External change".into();
    update(&mut core, &mailbox, external);
    assert_eq!(
        core.snapshot().queue.current.unwrap().title,
        "External change"
    );
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Disconnect,
    })
    .unwrap();
    assert_eq!(core.snapshot().state, PlaybackState::Paused);
    assert!(!core.snapshot().playback_session_active);
    assert_eq!(core.position, 500);
}
#[test]
fn local_audio_waits_for_confirmed_remote_pause() {
    let (mut core, mailbox) = coordinator();
    core.send(local_selection()).unwrap();
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("fixture"));
    assert_eq!(
        core.queue.snapshot().current.unwrap().id,
        demo_tracks()[0].id
    );
    assert!(!core.provider_loaded);
    assert!(matches!(
        mailbox.lock().unwrap().actions.last(),
        Some(RemoteAction::Control {
            control: RemoteControl::Pause
        })
    ));
    // A repeated observation cannot confirm an asynchronously submitted command.
    update(&mut core, &mailbox, state(1, false));
    assert!(core.remote.active.is_some());
    update(&mut core, &mailbox, state(2, false));
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
    assert_eq!(core.snapshot().state, PlaybackState::Playing);
    assert_eq!(
        core.queue.snapshot().current.unwrap().id,
        demo_tracks()[1].id
    );
}
#[test]
fn local_audio_waits_for_owned_start_to_settle_then_pauses_the_new_playback() {
    let (mut core, mailbox) = coordinator();
    let mut paused = output_state(2, "owned", 1200, false);
    paused.owned_output_observed = true;
    paused.owned_output_control = true;
    update(&mut core, &mailbox, paused);
    mailbox.lock().unwrap().starting = true;
    let action_count = mailbox.lock().unwrap().actions.len();
    core.send(local_selection()).unwrap();
    core.tick(Duration::ZERO);
    assert!(!core.provider_loaded);
    assert_eq!(mailbox.lock().unwrap().actions.len(), action_count);

    mailbox.lock().unwrap().starting = false;
    let mut playing = output_state(3, "owned", 250, true);
    playing.owned_output_observed = true;
    playing.owned_output_control = true;
    update(&mut core, &mailbox, playing);
    assert!(!core.provider_loaded);
    assert!(matches!(
        mailbox.lock().unwrap().actions.last(),
        Some(RemoteAction::Control {
            control: RemoteControl::Pause
        })
    ));

    let mut paused = output_state(4, "owned", 300, false);
    paused.owned_output_observed = true;
    paused.owned_output_control = true;
    update(&mut core, &mailbox, paused);
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
    assert!(core.provider_loaded);
}
#[test]
fn offline_owned_switch_requires_a_fresh_pause_from_the_owned_device() {
    let (mut core, mailbox) = coordinator();
    let mut playing = output_state(2, "owned", 1200, true);
    playing.connection = ProviderConnectionState::Unavailable;
    playing.issue = Some(PlayerError::unavailable("fixture"));
    playing.owned_output_observed = true;
    playing.owned_output_control = true;
    update(&mut core, &mailbox, playing);
    assert!(core.snapshot().capabilities.pause);
    core.send(local_selection()).unwrap();
    let mut wrong = output_state(3, "other", 1200, false);
    wrong.owned_output_observed = true;
    wrong.owned_output_control = true;
    update(&mut core, &mailbox, wrong);
    assert!(!core.provider_loaded);
    let mut paused = output_state(4, "owned", 1200, false);
    paused.connection = ProviderConnectionState::Unavailable;
    paused.owned_output_observed = true;
    paused.owned_output_control = true;
    update(&mut core, &mailbox, paused);
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
    assert!(core.provider_loaded);
}
#[test]
fn new_transport_cancels_pending_switch_and_late_library_intent() {
    let (mut core, mailbox) = coordinator();
    core.send(local_selection()).unwrap();
    let token = core.library_play_token;
    core.send(PlayerCommand::Play).unwrap();
    assert!(core.library_play_token != token);
    update(&mut core, &mailbox, state(2, false));
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("fixture"));
    assert!(!core.provider_loaded);
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Activate,
    })
    .unwrap();
    assert!(core.library_play_token != token);
}
#[test]
fn failed_or_timed_out_pause_never_starts_local_audio() {
    for timeout in [true, false] {
        let (mut core, mailbox) = coordinator();
        core.send(local_selection()).unwrap();
        if timeout {
            core.remote.pending_local.as_mut().unwrap().deadline =
                Instant::now() - Duration::from_secs(1);
            core.tick(Duration::ZERO);
        } else {
            let mut failed = state(2, true);
            failed.connection = ProviderConnectionState::Unavailable;
            failed.issue = Some(PlayerError::unavailable("fixture"));
            update(&mut core, &mailbox, failed);
        }
        assert!(!core.provider_loaded);
        assert!(core.remote.pending_local.is_none());
        assert_eq!(core.snapshot().active_provider.as_deref(), Some("fixture"));
        assert!(
            core.events
                .iter()
                .any(|e| matches!(e, PlayerEvent::Error { .. }))
        );
    }
}
#[test]
fn remote_clock_anchors_only_on_fresh_observations_and_restrictions_apply() {
    let (mut core, mailbox) = coordinator();
    core.remote.anchors.insert(
        "fixture".into(),
        (1, Instant::now() - Duration::from_millis(350)),
    );
    let before = core.snapshot().position_ms;
    assert!(before >= 1350);
    core.send(PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Visibility { visible: false },
    })
    .unwrap();
    update(&mut core, &mailbox, state(1, true));
    assert!(core.snapshot().position_ms >= before);
    let mut restricted = state(2, true);
    restricted.playback.as_mut().unwrap().capabilities.seek = false;
    update(&mut core, &mailbox, restricted);
    assert!(core.snapshot().position_ms < 1200);
    assert!(core.send(PlayerCommand::Seek { position_ms: 500 }).is_err());
    assert!(core.send(PlayerCommand::Stop).is_err());
}

#[test]
fn lost_connection_keeps_last_observed_state_without_claiming_remote_pause() {
    let (mut core, mailbox) = coordinator();
    let mut lost = state(1, true);
    lost.connection = ProviderConnectionState::Unavailable;
    lost.issue = Some(PlayerError::new(ErrorCode::NetworkError, "Fixture offline"));
    update(&mut core, &mailbox, lost);
    let snapshot = core.snapshot();
    assert_eq!(snapshot.state, PlaybackState::Playing);
    assert_eq!(snapshot.position_ms, 1000);
    assert!(!snapshot.capabilities.pause);
    assert_eq!(
        snapshot.provider_connection,
        ProviderConnectionState::Unavailable
    );
}

#[test]
fn remote_position_uses_the_reported_speed_and_paused_clock_stays_frozen() {
    let (mut core, mailbox) = coordinator();
    let mut faster = state(2, true);
    faster.playback.as_mut().unwrap().position_rate = 2.0;
    update(&mut core, &mailbox, faster);
    core.remote.anchors.insert(
        "fixture".into(),
        (2, Instant::now() - Duration::from_millis(400)),
    );
    let snapshot = core.snapshot();
    assert_eq!(snapshot.playback_rate, 2.0);
    assert!(snapshot.position_ms >= 1800 && snapshot.position_ms < 2200);
    let mut paused = state(3, false);
    paused.playback.as_mut().unwrap().position_rate = 0.0;
    update(&mut core, &mailbox, paused);
    core.remote.anchors.insert(
        "fixture".into(),
        (3, Instant::now() - Duration::from_secs(2)),
    );
    assert_eq!(core.snapshot().position_ms, 1000);
}

#[test]
fn a_playing_clock_cannot_extrapolate_to_eof_in_the_saved_selection() {
    for (playing, observed, expected) in [
        (true, 179_750, 179_750),
        (false, 180_000, 180_000),
        (true, 180_000, 180_000),
    ] {
        let (mut core, network, storage) = session_restore_setup();
        storage.lock().unwrap().updates.extend([
            SessionUpdate::Loaded {
                session: None,
                issues: vec![],
            },
            SessionUpdate::SoundLoaded(Ok(SoundSettings::default())),
        ]);
        core.tick(Duration::ZERO);
        bind_profile(&mut core, &network, "original-account-a");
        core.send(PlayerCommand::Remote {
            provider: "fixture".into(),
            action: RemoteAction::Activate,
        })
        .unwrap();
        update(
            &mut core,
            &network,
            output_state(1, "owned", observed, playing),
        );
        core.remote.anchors.insert(
            "fixture".into(),
            (1, Instant::now() - Duration::from_millis(500)),
        );
        assert_eq!(core.snapshot().position_ms, 180_000);
        core.shutdown();
        let storage = storage.lock().unwrap();
        let saved = storage.selections.last().unwrap().as_ref().unwrap();
        assert_eq!(saved.position_ms, expected);
        assert_eq!(saved.current.id, saved_remote().current.id);
    }
}

#[test]
fn remote_queue_add_requires_its_capability_and_does_not_activate_the_remote_source() {
    let (mut core, mailbox) = coordinator();
    let mut blocked = state(2, false);
    blocked.playback.as_mut().unwrap().transport.queue_add = false;
    let item = blocked.playback.as_ref().unwrap().current.clone().unwrap();
    update(&mut core, &mailbox, blocked);
    let action = PlayerCommand::Remote {
        provider: "fixture".into(),
        action: RemoteAction::Control {
            control: RemoteControl::Enqueue { item },
        },
    };
    let count = mailbox.lock().unwrap().actions.len();
    assert!(core.send(action.clone()).is_err());
    assert_eq!(mailbox.lock().unwrap().actions.len(), count);
    update(&mut core, &mailbox, state(3, false));
    core.send(PlayerCommand::ReplaceQueue {
        items: demo_tracks(),
        start: 0,
        context: PlaybackContext::UserQueue,
    })
    .unwrap();
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
    core.send(action).unwrap();
    assert_eq!(core.snapshot().active_provider.as_deref(), Some("mock"));
}

#[test]
fn local_sound_preferences_save_while_remote_owns_transport_without_remote_commands() {
    struct SoundRecorder(Arc<Mutex<Option<SoundSettings>>>);
    impl SessionStore for SoundRecorder {
        fn submit(&mut self, action: SessionAction) -> Result<(), PlayerError> {
            if let SessionAction::SaveSound { settings, .. } = action {
                *self.0.lock().unwrap() = Some(settings);
            }
            Ok(())
        }
        fn poll(&mut self) -> Option<SessionUpdate> {
            None
        }
    }
    let (mut core, mailbox) = coordinator();
    let saved = Arc::new(Mutex::new(None));
    core.register_session_store(SoundRecorder(saved.clone()));
    core.sound.ready = true;
    let remote_id = core.snapshot().queue.current.unwrap().id;
    let local_id = core.queue.current().unwrap().id.clone();
    let commands = mailbox.lock().unwrap().actions.len();
    let mut settings = SoundSettings::default();
    settings.equalizer.enabled = true;
    settings.equalizer.bands_db[0] = 6.0;
    core.send(PlayerCommand::SetSound {
        settings: settings.clone(),
    })
    .unwrap();
    core.tick(Duration::from_millis(250));
    assert_eq!(*saved.lock().unwrap(), Some(settings));
    let snapshot = core.snapshot();
    assert_eq!(snapshot.queue.current.unwrap().id, remote_id);
    assert_eq!(core.queue.current().unwrap().id, local_id);
    assert!(!snapshot.capabilities.equalizer && !snapshot.capabilities.pcm_analysis);
    assert!(snapshot.output_statistics.is_none());
    assert_eq!(mailbox.lock().unwrap().actions.len(), commands);
    assert!(core.send(PlayerCommand::RetryPlayback).is_err());
}
